//! MCP servers the user configures once and Patchbay hands to every Code session through ACP's
//! standard `mcpServers` field, so any ACP agent can use them.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{Db, McpServer};
use crate::providers;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyValue {
    pub name: String,
    /// Blank for secrets once stored (the value lives in the keyring).
    pub value: String,
    pub secret: bool,
    /// Output only: a secret value is stored.
    pub stored: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct McpConfig {
    /// stdio | http | sse
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<KeyValue>,
    pub url: String,
    pub headers: Vec<KeyValue>,
}

impl McpConfig {
    pub fn validate(&self) -> Result<()> {
        match self.transport.as_str() {
            "stdio" if self.command.trim().is_empty() => Err(anyhow!("a stdio server needs a command")),
            "http" | "sse" if !self.url.starts_with("http://") && !self.url.starts_with("https://") => {
                Err(anyhow!("the server URL must start with http:// or https://"))
            }
            "stdio" | "http" | "sse" => Ok(()),
            _ => Err(anyhow!("transport must be stdio, http or sse")),
        }
    }
}

/// Transports the running agent accepts (`agentCapabilities.mcpCapabilities`); stdio is mandatory.
#[derive(Debug, Clone, Copy, Default)]
pub struct McpSupport {
    pub http: bool,
    pub sse: bool,
}

impl McpSupport {
    pub fn from_init(init: &Value) -> Self {
        let caps = &init["agentCapabilities"]["mcpCapabilities"];
        Self {
            http: caps["http"].as_bool().unwrap_or(false),
            sse: caps["sse"].as_bool().unwrap_or(false),
        }
    }
}

pub(crate) fn secret_account(server_id: &str, section: &str, name: &str) -> String {
    format!("mcp:{server_id}:{section}:{name}")
}

/// Persist a server. Secret values move to the keyring (or stay inline when there is none);
/// a blank secret keeps the previously stored value.
pub fn save(db: &Db, id: Option<String>, name: &str, enabled: bool, mut cfg: McpConfig) -> Result<McpServer> {
    cfg.transport = cfg.transport.trim().to_lowercase();
    cfg.validate()?;
    if name.trim().is_empty() {
        return Err(anyhow!("name is required"));
    }
    let id = id.unwrap_or_else(crate::db::new_id);
    let previous: Option<McpConfig> = db
        .list_mcp_servers()?
        .into_iter()
        .find(|s| s.id == id)
        .and_then(|s| serde_json::from_value(s.config).ok());
    for (section, list) in [("env", &mut cfg.env), ("header", &mut cfg.headers)] {
        list.retain(|kv| !kv.name.trim().is_empty());
        for kv in list.iter_mut() {
            kv.name = kv.name.trim().to_owned();
            kv.stored = false;
            if !kv.secret {
                continue;
            }
            let account = secret_account(&id, section, &kv.name);
            if kv.value.is_empty() {
                // Keep what is stored (keyring or the previous inline fallback).
                let prev_inline = previous
                    .as_ref()
                    .map(|p| if section == "env" { &p.env } else { &p.headers })
                    .and_then(|l| l.iter().find(|p| p.name == kv.name && p.secret))
                    .map(|p| p.value.clone())
                    .unwrap_or_default();
                kv.value = prev_inline;
            } else if providers::keyring_set(&account, &kv.value) {
                kv.value.clear();
            }
        }
    }
    let server = McpServer {
        id,
        name: name.trim().to_owned(),
        enabled,
        config: serde_json::to_value(&cfg)?,
    };
    db.upsert_mcp_server(&server)?;
    Ok(redact(server))
}

pub fn delete(db: &Db, id: &str) -> Result<()> {
    if let Some(s) = db.list_mcp_servers()?.into_iter().find(|s| s.id == id)
        && let Ok(cfg) = serde_json::from_value::<McpConfig>(s.config)
    {
        for (section, list) in [("env", &cfg.env), ("header", &cfg.headers)] {
            for kv in list.iter().filter(|kv| kv.secret) {
                providers::keyring_delete(&secret_account(id, section, &kv.name));
            }
        }
    }
    db.delete_mcp_server(id)
}

fn secret_value(server_id: &str, section: &str, kv: &KeyValue) -> Option<String> {
    if !kv.value.is_empty() {
        return Some(kv.value.clone());
    }
    providers::keyring_get(&secret_account(server_id, section, &kv.name))
}

/// For the UI: blank inline secrets and flag which ones have a value.
pub fn redact(mut s: McpServer) -> McpServer {
    if let Ok(mut cfg) = serde_json::from_value::<McpConfig>(s.config.clone()) {
        for (section, list) in [("env", &mut cfg.env), ("header", &mut cfg.headers)] {
            for kv in list.iter_mut().filter(|kv| kv.secret) {
                kv.stored = secret_value(&s.id, section, kv).is_some();
                kv.value.clear();
            }
        }
        s.config = serde_json::to_value(cfg).unwrap_or_default();
    }
    s
}

pub fn list_redacted(db: &Db) -> Result<Vec<McpServer>> {
    Ok(db.list_mcp_servers()?.into_iter().map(redact).collect())
}

/// ACP server names must match `^[a-zA-Z0-9_-]+$` for agents with strict schemas (e.g. Antigravity).
pub fn acp_server_name(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "mcp-server".into()
    } else {
        trimmed.to_owned()
    }
}

/// Enabled servers as ACP `McpServer` values, with secrets filled in. Servers on a transport
/// the agent doesn't support are returned separately so the UI can say why they're missing.
pub fn acp_servers(db: &Db, support: McpSupport) -> Result<(Vec<Value>, Vec<String>)> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for s in db.list_mcp_servers()?.into_iter().filter(|s| s.enabled) {
        let Ok(cfg) = serde_json::from_value::<McpConfig>(s.config.clone()) else { continue };
        let pairs = |section: &str, list: &[KeyValue]| -> Vec<Value> {
            list.iter()
                .filter_map(|kv| {
                    let v = if kv.secret { secret_value(&s.id, section, kv)? } else { kv.value.clone() };
                    Some(json!({ "name": kv.name, "value": v }))
                })
                .collect()
        };
        let acp_name = acp_server_name(&s.name);
        match cfg.transport.as_str() {
            "stdio" => out.push(json!({
                "name": acp_name,
                "command": cfg.command,
                "args": cfg.args,
                "env": pairs("env", &cfg.env),
            })),
            t @ ("http" | "sse") => {
                if (t == "http" && !support.http) || (t == "sse" && !support.sse) {
                    skipped.push(format!("{} ({t})", s.name));
                    continue;
                }
                out.push(json!({
                    "type": t,
                    "name": acp_name,
                    "url": cfg.url,
                    "headers": pairs("header", &cfg.headers),
                }));
            }
            _ => {}
        }
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_shapes_and_transport_filtering() {
        let dir = std::env::temp_dir().join(format!("pb-mcp-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let stdio = McpConfig {
            transport: "stdio".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "server-fs".into()],
            env: vec![KeyValue { name: "ROOT".into(), value: "/tmp".into(), ..Default::default() }],
            ..Default::default()
        };
        save(&db, None, "fs", true, stdio).unwrap();
        let http = McpConfig { transport: "http".into(), url: "https://example.com/mcp".into(), ..Default::default() };
        save(&db, None, "remote", true, http).unwrap();
        save(&db, None, "off", false, McpConfig { transport: "stdio".into(), command: "x".into(), ..Default::default() }).unwrap();

        let (servers, skipped) = acp_servers(&db, McpSupport::default()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["command"], "npx");
        assert_eq!(servers[0]["env"][0]["name"], "ROOT");
        assert!(servers[0].get("type").is_none(), "stdio is the untagged variant");
        assert_eq!(skipped, vec!["remote (http)".to_string()]);
        // The ACP crate accepts what we build.
        let _: Vec<agent_client_protocol::McpServer> = serde_json::from_value(Value::Array(servers)).unwrap();

        let (servers, _) = acp_servers(&db, McpSupport { http: true, sse: false }).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1]["type"], "http");
    }

    #[test]
    fn acp_server_name_slugifies_spaces_and_special_chars() {
        assert_eq!(acp_server_name("Patchbay agent messaging"), "Patchbay-agent-messaging");
        assert_eq!(acp_server_name("my_server-1"), "my_server-1");
        assert_eq!(acp_server_name("  spaced  server  "), "spaced--server");
        assert_eq!(acp_server_name("!@#$%"), "mcp-server");
    }

    #[test]
    fn rejects_bad_config() {
        assert!(McpConfig { transport: "stdio".into(), ..Default::default() }.validate().is_err());
        assert!(McpConfig { transport: "http".into(), url: "ftp://x".into(), ..Default::default() }.validate().is_err());
        assert!(McpConfig { transport: "carrier-pigeon".into(), ..Default::default() }.validate().is_err());
    }
}
