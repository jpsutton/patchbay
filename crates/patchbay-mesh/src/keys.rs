//! Key material: where secrets are stored, and a node's long-lived identity keys.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Secret storage: the OS keyring on desktops, the Android Keystore on the phone, memory in tests.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    /// False when the value could not be stored.
    fn set(&self, account: &str, value: &str) -> bool;
    fn delete(&self, account: &str);
}

#[derive(Default)]
pub struct MemKeyStore(std::sync::Mutex<HashMap<String, String>>);

impl KeyStore for MemKeyStore {
    fn get(&self, account: &str) -> Option<String> {
        self.0.lock().unwrap().get(account).cloned()
    }
    fn set(&self, account: &str, value: &str) -> bool {
        self.0.lock().unwrap().insert(account.to_owned(), value.to_owned());
        true
    }
    fn delete(&self, account: &str) {
        self.0.lock().unwrap().remove(account);
    }
}

/// Write a file only its owner can read, replacing it atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// A node's identity: an X25519 key for Noise and an Ed25519 key for signing records.
pub struct Identity {
    pub noise_private: [u8; 32],
    pub noise_public: [u8; 32],
    pub signing: SigningKey,
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    noise_private: String,
    noise_public: String,
    sign_private: String,
}

impl Identity {
    /// Load `mesh/identity.json`, creating it on first use.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("mesh");
        let path = dir.join("identity.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            return Self::from_json(&raw).context("mesh identity file is corrupt");
        }
        let id = Self::generate()?;
        std::fs::create_dir_all(&dir)?;
        write_private(&path, id.to_json().as_bytes())?;
        Ok(id)
    }

    pub fn generate() -> Result<Self> {
        let kp = snow::Builder::new(crate::handshake::NOISE_PARAMS.parse()?).generate_keypair()?;
        Ok(Self {
            noise_private: kp.private.as_slice().try_into().map_err(|_| anyhow!("bad key length"))?,
            noise_public: kp.public.as_slice().try_into().map_err(|_| anyhow!("bad key length"))?,
            signing: SigningKey::from_bytes(&random32()),
        })
    }

    pub fn to_json(&self) -> String {
        let f = IdentityFile {
            noise_private: hex::encode(self.noise_private),
            noise_public: hex::encode(self.noise_public),
            sign_private: hex::encode(self.signing.to_bytes()),
        };
        serde_json::to_string_pretty(&f).unwrap_or_default()
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        let f: IdentityFile = serde_json::from_str(raw)?;
        Ok(Self {
            noise_private: hex32(&f.noise_private)?,
            noise_public: hex32(&f.noise_public)?,
            signing: SigningKey::from_bytes(&hex32(&f.sign_private)?),
        })
    }

    pub fn sign_pub_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    pub fn noise_pub_hex(&self) -> String {
        hex::encode(self.noise_public)
    }

    pub fn sign(&self, msg: &[u8]) -> String {
        hex::encode(self.signing.sign(msg).to_bytes())
    }
}

pub fn hex32(s: &str) -> Result<[u8; 32]> {
    hex::decode(s)?.try_into().map_err(|_| anyhow!("expected 32 bytes"))
}

pub fn verify(sign_pub_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Ok(key) = hex32(sign_pub_hex) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key) else { return false };
    let Ok(sig) = hex::decode(sig_hex) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else { return false };
    key.verify(msg, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}
