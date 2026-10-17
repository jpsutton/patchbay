//! Secrets on the phone (its identity keys, the mesh passphrase, resume tickets), each sealed
//! with a key the Android Keystore holds. Where no keystore exists (desktop test builds), values
//! stay in a file only the user can read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use patchbay_mesh::keys::{KeyStore, write_private};

type Seal = Box<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;
type Open = Box<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

pub struct SealedKeyStore {
    path: PathBuf,
    map: Mutex<HashMap<String, String>>,
    seal: Seal,
    open: Open,
}

impl SealedKeyStore {
    pub fn new(path: PathBuf, seal: Seal, open: Open) -> Self {
        let map = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { path, map: Mutex::new(map), seal, open }
    }

    fn write(&self, map: &HashMap<String, String>) -> bool {
        write_private(&self.path, serde_json::to_string(map).unwrap_or_default().as_bytes()).is_ok()
    }
}

impl KeyStore for SealedKeyStore {
    fn get(&self, account: &str) -> Option<String> {
        let v = self.map.lock().unwrap().get(account).cloned()?;
        if let Some(ct) = v.strip_prefix("k:") {
            return (self.open)(ct).map_err(|e| tracing::warn!("keystore: {e}")).ok();
        }
        v.strip_prefix("p:").map(str::to_owned)
    }

    fn set(&self, account: &str, value: &str) -> bool {
        let stored = match (self.seal)(value) {
            Ok(Some(ct)) => format!("k:{ct}"),
            Ok(None) => format!("p:{value}"),
            Err(e) => {
                tracing::warn!("keystore: {e}");
                return false;
            }
        };
        let mut map = self.map.lock().unwrap();
        map.insert(account.to_owned(), stored);
        self.write(&map)
    }

    fn delete(&self, account: &str) {
        let mut map = self.map.lock().unwrap();
        if map.remove(account).is_some() {
            self.write(&map);
        }
    }
}
