//! Key material: where secrets are stored, and this node's long-lived identity keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[cfg(test)]
pub use patchbay_mesh::keys::MemKeyStore;
pub use patchbay_mesh::keys::{Identity, KeyStore, hex32, random32, write_private};

/// The OS keyring, with a private file as fallback for the mesh's own secrets when no keyring
/// service runs (headless Linux).
pub struct OsKeyStore {
    fallback: PathBuf,
    /// Set for any data folder but the app's default one. The keyring is shared by every
    /// Patchbay on the computer, so another instance (`patchbayd --data-dir`, a scratch build)
    /// must not store its mesh secrets under the names the app uses.
    scope: Option<String>,
}

impl OsKeyStore {
    pub fn new(data_dir: &Path) -> Self {
        let scope = crate::default_data_dir().ok().and_then(|d| keyring_scope(data_dir, &d));
        Self { fallback: data_dir.join("mesh").join("secrets.json"), scope }
    }

    /// The keyring account for `account`. Only the mesh's own secrets are scoped: provider and
    /// MCP keys are named by their synced ids and read straight from the keyring elsewhere.
    fn keyring_account(&self, account: &str) -> String {
        match &self.scope {
            Some(s) if account.starts_with("mesh:") => format!("{account}#{s}"),
            _ => account.to_owned(),
        }
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

/// None for the default data folder, which keeps the unscoped names existing installs use;
/// otherwise a short tag derived from the folder's path.
fn keyring_scope(data_dir: &Path, default_dir: &Path) -> Option<String> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    let dir = canon(data_dir);
    if dir == canon(default_dir) {
        return None;
    }
    let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
    Some(hex::encode(&digest[..8]))
}

impl KeyStore for OsKeyStore {
    fn get(&self, account: &str) -> Option<String> {
        crate::providers::keyring_get(&self.keyring_account(account)).or_else(|| self.read_fallback().remove(account))
    }

    fn set(&self, account: &str, value: &str) -> bool {
        if crate::providers::keyring_set(&self.keyring_account(account), value) {
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
        crate::providers::keyring_delete(&self.keyring_account(account));
        let mut m = self.read_fallback();
        if m.remove(account).is_some() {
            self.write_fallback(&m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(data_dir: &Path, default_dir: &Path) -> OsKeyStore {
        OsKeyStore { fallback: data_dir.join("mesh").join("secrets.json"), scope: keyring_scope(data_dir, default_dir) }
    }

    #[test]
    fn default_folder_keeps_the_unscoped_names() {
        let base = std::env::temp_dir().join(format!("pb-keys-{}", crate::db::new_id()));
        let default_dir = base.join("dev.patchbay.app");
        std::fs::create_dir_all(&default_dir).unwrap();
        // A path through `..` still names the same folder.
        let same = base.join("x").join("..").join("dev.patchbay.app");
        std::fs::create_dir_all(base.join("x")).unwrap();
        let s = store(&same, &default_dir);
        assert_eq!(s.keyring_account("mesh:passphrase"), "mesh:passphrase");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn other_folders_get_their_own_mesh_names() {
        let base = std::env::temp_dir().join(format!("pb-keys-{}", crate::db::new_id()));
        let default_dir = base.join("dev.patchbay.app");
        let a = store(&base.join("daemon-a"), &default_dir);
        let b = store(&base.join("daemon-b"), &default_dir);
        let (pa, pb) = (a.keyring_account("mesh:passphrase"), b.keyring_account("mesh:passphrase"));
        assert!(pa.starts_with("mesh:passphrase#"));
        assert_ne!(pa, "mesh:passphrase");
        assert_ne!(pa, pb);
        assert_ne!(a.keyring_account("mesh:data_key"), b.keyring_account("mesh:data_key"));
        // Provider keys keep their ids: the providers module reads them by id directly.
        assert_eq!(a.keyring_account("prov-1"), "prov-1");
    }
}
