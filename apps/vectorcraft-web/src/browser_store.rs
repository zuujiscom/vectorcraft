//! Data Recovery's store on the web: IndexedDB (through craft-libs' `craft-web-recovery` map),
//! which holds far more than the 5 MB of local storage that all of the site's apps shared.
//!
//! The engine's [`RecoveryStore`] is synchronous, IndexedDB is not. So the store works on an
//! in-memory copy of the entries, read once at startup ([`BrowserStore::open`], before the first
//! frame, which looks for copies a crash left behind); every change updates the copy at once and
//! is queued for IndexedDB, which commits in the background and in order. A write that failed (a
//! full disk) is reported by the next call that writes. Other tabs announce what they put or
//! deleted, so a copy another tab restored or discarded is gone here too, and an area another tab
//! just took is never picked for a new one.
//!
//! No locks: each tab holds its area with a Web Lock named after it ([`WebLocks`]), which a tab
//! whose timers are paused in the background keeps, and with a heartbeat where the browser has no
//! Web Locks.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use craft_web_recovery::{ForeignChange, Kv};
use vectorcraft_engine::cmd::recovery::{Hold, RecoveryStore};

use crate::locks::WebLocks;

/// The Web Locks' names start with it, and so did the local-storage entries older versions kept.
pub const RECOVERY_PREFIX: &str = "vectorcraft-recovery/";
const DATABASE: &str = "vectorcraft.recovery";

thread_local! {
    /// Every entry (`<area>/<name>`) as this tab knows it.
    static ENTRIES: RefCell<BTreeMap<String, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    /// Entries other tabs stored after this one started: their names only.
    static FOREIGN: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    /// The IndexedDB connection (wasm runs on one thread), or why there is none.
    static KV: RefCell<Option<Result<Kv, String>>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub struct BrowserStore {
    locks: Option<WebLocks>,
}

impl BrowserStore {
    pub async fn open(locks: Option<WebLocks>) -> Self {
        let kv = match craft_web_recovery::open_kv(DATABASE, |change| match change {
            ForeignChange::Put(name) => FOREIGN.with(|f| {
                f.borrow_mut().insert(name);
            }),
            ForeignChange::Delete(name) => {
                ENTRIES.with(|e| e.borrow_mut().remove(&name));
                FOREIGN.with(|f| f.borrow_mut().remove(&name));
            }
        })
        .await
        {
            Ok((kv, entries)) => {
                ENTRIES.with(|e| e.borrow_mut().extend(entries));
                Ok(kv)
            }
            Err(e) => {
                log::warn!("Data Recovery has no browser storage: {e}");
                Err(e)
            }
        };
        KV.with(|k| *k.borrow_mut() = Some(kv));
        let store = Self { locks };
        store.migrate_local_storage();
        store
    }

    /// Copies older versions kept in local storage move to IndexedDB (and leave local storage).
    fn migrate_local_storage(&self) {
        let Ok(kv) = Self::kv() else { return };
        let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) else { return };
        let len = storage.length().unwrap_or(0);
        let keys: Vec<String> = (0..len).filter_map(|i| storage.key(i).ok().flatten()).filter(|k| k.starts_with(RECOVERY_PREFIX)).collect();
        for key in keys {
            let name = key[RECOVERY_PREFIX.len()..].to_string();
            if let Some(bytes) = storage.get_item(&key).ok().flatten().and_then(|t| vectorcraft_format::base64_decode(&t)) {
                ENTRIES.with(|e| e.borrow_mut().insert(name.clone(), bytes.clone()));
                kv.put(&name, &bytes);
            }
            let _ = storage.remove_item(&key);
        }
    }

    fn kv() -> Result<Kv, String> {
        KV.with(|k| match &*k.borrow() {
            Some(Ok(kv)) => Ok(kv.clone()),
            Some(Err(e)) => Err(format!("browser storage is unavailable: {e}")),
            None => Err("browser storage is not open".into()),
        })
    }
}

impl RecoveryStore for BrowserStore {
    fn list(&self) -> Result<Vec<String>, String> {
        Ok(ENTRIES.with(|e| e.borrow().keys().cloned().collect()))
    }
    /// Also the areas other tabs took since this one started, so two tabs opened in the same
    /// second never pick the same name.
    fn areas(&self) -> Result<Vec<String>, String> {
        let mut areas: BTreeSet<String> = ENTRIES.with(|e| e.borrow().keys().filter_map(|n| n.split_once('/').map(|(a, _)| a.to_string())).collect());
        FOREIGN.with(|f| areas.extend(f.borrow().iter().filter_map(|n| n.split_once('/').map(|(a, _)| a.to_string()))));
        Ok(areas.into_iter().collect())
    }
    fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        ENTRIES.with(|e| e.borrow().get(name).cloned()).ok_or_else(|| format!("{name}: no such recovery entry"))
    }
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let kv = Self::kv()?;
        if let Some(e) = kv.take_error() {
            return Err(format!("browser storage is full or turned off: {e}"));
        }
        ENTRIES.with(|e| e.borrow_mut().insert(name.to_string(), bytes.to_vec()));
        kv.put(name, bytes);
        Ok(())
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        ENTRIES.with(|e| e.borrow_mut().remove(name));
        FOREIGN.with(|f| f.borrow_mut().remove(name));
        if let Ok(kv) = Self::kv() {
            kv.delete(name);
        }
        Ok(())
    }
    fn location(&self) -> String {
        "browser storage".into()
    }
    fn now(&self) -> Option<i64> {
        Some((js_sys::Date::now() / 1000.0) as i64)
    }
    fn announce(&self, area: &str) -> Option<Hold> {
        self.locks.as_ref()?.hold(area)
    }
    fn announced(&self, area: &str) -> Option<bool> {
        self.locks.as_ref().map(|l| l.held(area))
    }
}
