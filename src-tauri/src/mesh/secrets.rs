//! Provider API keys and MCP secrets, synced sealed under the mesh data key.
//!
//! Locally a secret lives in the OS keyring (account = provider id, or `mcp:<server>:<section>:<name>`).
//! `reseal` copies each one into `mesh_secrets` as XChaCha20-Poly1305 ciphertext; `install` puts a
//! received one back into this node's keyring.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::keys::KeyStore;
use crate::db::Db;

#[derive(Clone)]
pub struct DataKey {
    pub generation: u32,
    pub key: [u8; 32],
}

impl DataKey {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.generation, hex::encode(self.key))
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (g, k) = s.split_once(':')?;
        Some(Self { generation: g.parse().ok()?, key: super::keys::hex32(k).ok()? })
    }

    /// Orders competing keys: a higher generation wins, ties go to the larger key.
    pub fn rank(&self) -> (u32, [u8; 32]) {
        (self.generation, self.key)
    }

    fn digest(&self, account: &str, value: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.key);
        h.update(account.as_bytes());
        h.update([0]);
        h.update(value.as_bytes());
        hex::encode(h.finalize())
    }
}

/// Every secret this node has, by keyring account.
pub fn local_secrets(db: &Db, keys: &dyn KeyStore) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (p, inline) in db.list_providers()? {
        if let Some(k) = inline.filter(|k| !k.is_empty()).or_else(|| keys.get(&p.id)) {
            out.insert(p.id.clone(), k);
        }
    }
    for s in db.list_mcp_servers()? {
        let cfg: crate::code::mcp::McpConfig = serde_json::from_value(s.config.clone()).unwrap_or_default();
        for (section, list) in [("env", &cfg.env), ("headers", &cfg.headers)] {
            for kv in list.iter().filter(|kv| kv.secret) {
                let account = crate::code::mcp::secret_account(&s.id, section, &kv.name);
                let v = if kv.value.is_empty() { keys.get(&account) } else { Some(kv.value.clone()) };
                if let Some(v) = v.filter(|v| !v.is_empty()) {
                    out.insert(account, v);
                }
            }
        }
    }
    Ok(out)
}

fn seal(dk: &DataKey, account: &str, value: &str) -> Result<(String, String)> {
    let cipher = XChaCha20Poly1305::new((&dk.key).into());
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), chacha20poly1305::aead::Payload { msg: value.as_bytes(), aad: account.as_bytes() })
        .map_err(|_| anyhow!("encrypt failed"))?;
    Ok((hex::encode(nonce), hex::encode(ct)))
}

fn open(dk: &DataKey, account: &str, nonce: &str, ct: &str) -> Option<String> {
    let cipher = XChaCha20Poly1305::new((&dk.key).into());
    let nonce = hex::decode(nonce).ok()?;
    if nonce.len() != 24 {
        return None;
    }
    let plain = cipher
        .decrypt(XNonce::from_slice(&nonce), chacha20poly1305::aead::Payload { msg: &hex::decode(ct).ok()?, aad: account.as_bytes() })
        .ok()?;
    String::from_utf8(plain).ok()
}

struct Row {
    generation: u32,
    digest: String,
}

fn rows(db: &Db) -> Result<HashMap<String, Row>> {
    db.with_conn(|c| {
        let mut stmt = c.prepare("SELECT account, gen, digest FROM mesh_secrets")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, Row { generation: r.get(1)?, digest: r.get(2)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })
}

/// Bring `mesh_secrets` in line with this node's local secrets: seal new or changed ones (and
/// everything under a new data key), drop ones whose owner is gone.
pub fn reseal(db: &Db, keys: &dyn KeyStore, dk: &DataKey) -> Result<()> {
    let local = local_secrets(db, keys)?;
    let stored = rows(db)?;
    for (account, value) in &local {
        let digest = dk.digest(account, value);
        if stored.get(account).is_some_and(|r| r.generation == dk.generation && r.digest == digest) {
            continue;
        }
        let (nonce, ct) = seal(dk, account, value)?;
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO mesh_secrets (account, gen, nonce, ct, digest) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account) DO UPDATE SET gen = ?2, nonce = ?3, ct = ?4, digest = ?5",
                params![account, dk.generation, nonce, ct, digest],
            )
        })?;
    }
    // A secret is dropped only once its owner (provider or MCP server) is gone everywhere here.
    let providers: Vec<String> = db.list_providers()?.into_iter().map(|(p, _)| p.id).collect();
    let servers: Vec<String> = db.list_mcp_servers()?.into_iter().map(|s| s.id).collect();
    for account in stored.keys() {
        let owner_exists = match account.strip_prefix("mcp:") {
            Some(rest) => servers.iter().any(|s| rest.starts_with(&format!("{s}:"))),
            None => providers.contains(account),
        };
        if !owner_exists {
            db.with_conn(|c| c.execute("DELETE FROM mesh_secrets WHERE account = ?1", [account]))?;
        }
    }
    Ok(())
}

/// Put a received secret into the local keyring. Returns false when it can't be read yet (sealed
/// under a data key this node doesn't have).
pub fn install(db: &Db, keys: &dyn KeyStore, dk: &DataKey, account: &str) -> Result<bool> {
    let row: Option<(String, String)> = db.with_conn(|c| {
        c.query_row("SELECT nonce, ct FROM mesh_secrets WHERE account = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    })?;
    let Some((nonce, ct)) = row else {
        keys.delete(account);
        return Ok(true);
    };
    let Some(value) = open(dk, account, &nonce, &ct) else { return Ok(false) };
    if keys.get(account).as_deref() == Some(value.as_str()) {
        return Ok(true);
    }
    if !keys.set(account, &value) {
        let is_provider = db.list_providers()?.iter().any(|(p, _)| p.id == account);
        if is_provider {
            // Same fallback as a key typed into Settings on a machine without a keyring.
            db.with_conn(|c| c.execute("UPDATE providers SET inline_key = ?2 WHERE id = ?1", params![account, value]))?;
        } else {
            tracing::warn!(account, "no keyring available; synced MCP secret not stored");
        }
    }
    Ok(true)
}

/// Install every synced secret (after joining, or after receiving a newer data key).
pub fn install_all(db: &Db, keys: &dyn KeyStore, dk: &DataKey) -> Result<()> {
    for account in rows(db)?.into_keys() {
        install(db, keys, dk, &account)?;
    }
    Ok(())
}

/// An MCP config as it goes to peers: inline secret values (stored inline only when there is no
/// keyring) are blanked, since secrets travel sealed in `mesh_secrets` instead.
pub fn redact_mcp_config(v: &serde_json::Value) -> serde_json::Value {
    let Some(s) = v.as_str() else { return v.clone() };
    let Ok(mut cfg) = serde_json::from_str::<serde_json::Value>(s) else { return v.clone() };
    for section in ["env", "headers"] {
        if let Some(list) = cfg.get_mut(section).and_then(|l| l.as_array_mut()) {
            for kv in list {
                if kv["secret"].as_bool() == Some(true) {
                    kv["value"] = serde_json::Value::String(String::new());
                }
            }
        }
    }
    serde_json::Value::String(cfg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::keys::{MemKeyStore, random32};

    #[test]
    fn seal_and_open_round_trip() {
        let dk = DataKey { generation: 1, key: random32() };
        let (n, ct) = seal(&dk, "acct", "sk-123").unwrap();
        assert_eq!(open(&dk, "acct", &n, &ct).as_deref(), Some("sk-123"));
        assert_eq!(open(&dk, "other", &n, &ct), None, "bound to its account");
        let other = DataKey { generation: 2, key: random32() };
        assert_eq!(open(&other, "acct", &n, &ct), None);
    }

    #[test]
    fn provider_key_moves_between_nodes() {
        let dir = |n: &str| {
            let d = std::env::temp_dir().join(format!("pb-sec-{n}-{}", crate::db::new_id()));
            std::fs::create_dir_all(&d).unwrap();
            d.join("db.sqlite")
        };
        let (a, b) = (Db::open(&dir("a")).unwrap(), Db::open(&dir("b")).unwrap());
        let (ka, kb) = (MemKeyStore::default(), MemKeyStore::default());
        let dk = DataKey { generation: 1, key: random32() };
        let p = crate::db::Provider {
            id: "prov1".into(),
            name: "P".into(),
            kind: crate::db::ProviderKind::OpenaiChat,
            base_url: "http://x".into(),
            has_key: false,
            auth_mode: Default::default(),
        };
        a.upsert_provider(&p, None).unwrap();
        ka.set("prov1", "sk-secret");
        reseal(&a, &ka, &dk).unwrap();
        let ops = a.with_conn(|c| super::super::sync::ops_since(c, &Default::default(), 1000)).unwrap().ops;
        assert!(!serde_json::to_string(&ops).unwrap().contains("sk-secret"));
        b.with_conn(|c| super::super::sync::apply(c, &ops, &())).unwrap();
        install_all(&b, &kb, &dk).unwrap();
        assert_eq!(kb.get("prov1").as_deref(), Some("sk-secret"));
        // Unchanged secrets aren't rewritten.
        let before = a.with_conn(|c| super::super::sync::version_vector(c)).unwrap();
        reseal(&a, &ka, &dk).unwrap();
        assert_eq!(a.with_conn(|c| super::super::sync::version_vector(c)).unwrap(), before);
    }
}
