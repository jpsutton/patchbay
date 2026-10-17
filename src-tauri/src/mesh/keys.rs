//! Key material: where secrets are stored, and this node's long-lived identity keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub use patchbay_mesh::keys::MemKeyStore;
pub use patchbay_mesh::keys::{Identity, KeyStore, hex32, random32, write_private};

/// The OS keyring, with a private file as fallback for the mesh's own secrets when no keyring
/// service runs (headless Linux).
pub struct OsKeyStore {
    fallback: PathBuf,
}

impl OsKeyStore {
    pub fn new(data_dir: &Path) -> Self {
        Self { fallback: data_dir.join("mesh").join("secrets.json") }
    }

    fn read_fallback(&self) -> HashMap<String, String> {
        std::fs::read_to_string(&self.fallback)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_fallback(&self, m: &HashMap<String, String>) -> bool {
        let Some(dir) = self.fallback.parent() else { return false };
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        write_private(&self.fallback, serde_json::to_string(m).unwrap_or_default().as_bytes()).is_ok()
    }
}

impl KeyStore for OsKeyStore {
    fn get(&self, account: &str) -> Option<String> {
        crate::providers::keyring_get(account).or_else(|| self.read_fallback().remove(account))
    }

    fn set(&self, account: &str, value: &str) -> bool {
        if crate::providers::keyring_set(account, value) {
            return true;
        }
        // Only the mesh's own secrets fall back to a file; other callers keep their own fallbacks.
        if account.starts_with("mesh:") {
            let mut m = self.read_fallback();
            m.insert(account.to_owned(), value.to_owned());
            return self.write_fallback(&m);
        }
        false
    }

    fn delete(&self, account: &str) {
        crate::providers::keyring_delete(account);
        let mut m = self.read_fallback();
        if m.remove(account).is_some() {
            self.write_fallback(&m);
        }
    }
}
