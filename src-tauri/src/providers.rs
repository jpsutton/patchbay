//! Provider configuration: secrets, model discovery, mapping to `patchbay_llm`'s `Config`.

use anyhow::{Context, Result, anyhow};
use patchbay_llm::{AuthScheme, SystemBlock, WireFormat};
use serde::{Deserialize, Serialize};

use crate::db::{AuthMode, Db, Model, Provider, ProviderKind};

const KEYRING_SERVICE: &str = "dev.patchbay.app";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Store a provider API key in the OS keyring. Falls back to the DB when no keyring service is
/// available (e.g. headless Linux without a Secret Service daemon).
pub fn store_key(db: &Db, provider: &Provider, key: &str) -> Result<()> {
    let stored = keyring::Entry::new(KEYRING_SERVICE, &provider.id)
        .and_then(|e| e.set_password(key))
        .is_ok();
    if stored {
        db.clear_inline_key(&provider.id)?;
        db.upsert_provider(provider, None)?;
    } else {
        tracing::warn!(provider = %provider.id, "keyring unavailable; storing key in database");
        db.upsert_provider(provider, Some(key))?;
    }
    Ok(())
}

pub fn delete_key(provider_id: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, provider_id) {
        let _ = e.delete_credential();
    }
}

pub fn load_key(provider_id: &str, inline: Option<&str>) -> Option<String> {
    if let Some(k) = inline.filter(|k| !k.is_empty()) {
        return Some(k.to_owned());
    }
    keyring::Entry::new(KEYRING_SERVICE, provider_id)
        .and_then(|e| e.get_password())
        .ok()
        .filter(|k| !k.is_empty())
}

/// Store a secret under `account`; false when no keyring is available.
pub fn keyring_set(account: &str, value: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .and_then(|e| e.set_password(value))
        .is_ok()
}

pub fn keyring_get(account: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .and_then(|e| e.get_password())
        .ok()
        .filter(|v| !v.is_empty())
}

pub fn keyring_delete(account: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, account) {
        let _ = e.delete_credential();
    }
}

pub fn list_with_key_flag(db: &Db) -> Result<Vec<Provider>> {
    Ok(db
        .list_providers()?
        .into_iter()
        .map(|(mut p, inline)| {
            p.has_key = load_key(&p.id, inline.as_deref()).is_some();
            p
        })
        .collect())
}

pub fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

pub fn wire_format(kind: &ProviderKind) -> WireFormat {
    match kind {
        ProviderKind::OpenaiChat => WireFormat::ChatCompletions,
        ProviderKind::OpenaiResponses => WireFormat::Responses,
        ProviderKind::Anthropic => WireFormat::Anthropic,
        ProviderKind::Gemini => WireFormat::Gemini,
    }
}

/// How a resolved provider authenticates for sampling / discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Key,
    ClaudeLogin,
}

/// Resolved model + provider + key, ready to sample.
pub struct Resolved {
    pub provider: Provider,
    pub model: Model,
    pub key: Option<String>,
    pub auth: Auth,
}

pub fn resolve(db: &Db, model_ref: &str) -> Result<Resolved> {
    let model = db
        .get_model(model_ref)?
        .ok_or_else(|| anyhow!("model {model_ref} not found; pick a model in Settings"))?;
    let (provider, inline) = db
        .get_provider(&model.provider_id)?
        .ok_or_else(|| anyhow!("provider for model {} missing", model.display_name))?;
    let auth = match provider.auth_mode {
        AuthMode::ClaudeLogin => Auth::ClaudeLogin,
        AuthMode::ApiKey => Auth::Key,
    };
    let key = match auth {
        Auth::ClaudeLogin => None,
        Auth::Key => load_key(&provider.id, inline.as_deref()),
    };
    Ok(Resolved {
        provider,
        model,
        key,
        auth,
    })
}

/// Fill `resolved.key` for Claude login providers (no-op for API-key auth).
pub async fn ensure_token(resolved: &mut Resolved) -> Result<()> {
    if resolved.auth != Auth::ClaudeLogin {
        return Ok(());
    }
    resolved.key = Some(crate::claude_auth::access_token().await?);
    Ok(())
}

/// Subscription OAuth tokens only serve requests whose first system block is exactly this line.
/// Anything else (no system prompt, or this text merged into a longer block) gets a bare
/// `429 rate_limit_error: Error`.
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// System blocks the provider needs ahead of the caller's own.
pub fn system_prefix(r: &Resolved) -> Vec<SystemBlock> {
    // Its own block, so the caller's system prompt follows untouched.
    if r.auth == Auth::ClaudeLogin { vec![SystemBlock::new(CLAUDE_CODE_IDENTITY)] } else { Vec::new() }
}

/// Make `req` acceptable to the provider before sending it.
pub fn prepare_request(r: &Resolved, req: &mut patchbay_llm::Request) {
    for (i, b) in system_prefix(r).into_iter().enumerate() {
        req.system.insert(i, b);
    }
}

pub fn llm_config(r: &Resolved) -> patchbay_llm::Config {
    let mut cfg = patchbay_llm::Config::new(
        wire_format(&r.provider.kind),
        normalize_base_url(&r.provider.base_url),
        r.model.model_id.clone(),
    );
    cfg.key = r.key.clone();
    if r.provider.kind == ProviderKind::Anthropic {
        match r.auth {
            Auth::ClaudeLogin => {
                cfg.auth = AuthScheme::Bearer;
                cfg.extra_headers.push(("anthropic-beta".into(), ANTHROPIC_OAUTH_BETA.into()));
            }
            Auth::Key => cfg.auth = AuthScheme::XApiKey,
        }
    }
    let p = &r.model.params;
    cfg.max_output_tokens = p.max_output_tokens;
    cfg.temperature = p.temperature;
    cfg.top_p = p.top_p;
    cfg.effort = p.reasoning_effort();
    cfg.summary = p.reasoning_summary();
    cfg
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: String,
    /// Context window when the endpoint reports one (OpenRouter, Fireworks, vLLM, Anthropic…).
    pub context_window: Option<u64>,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelListItem>,
}

#[derive(Deserialize)]
struct ModelListItem {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Endpoints spell the context window differently; take the first one present.
fn context_window_of(rest: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    const KEYS: &[&str] = &[
        "context_length",
        "context_window",
        "max_input_tokens",
        "max_context_length",
        "max_model_len",
        "input_token_limit",
    ];
    let num = |v: &serde_json::Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    KEYS.iter()
        .find_map(|k| rest.get(*k).and_then(num))
        .or_else(|| rest.get("top_provider")?.get("context_length").and_then(num))
        .filter(|n| *n > 0)
}

/// Gemini lists `{models: [{name: "models/x", displayName, inputTokenLimit, ...}]}`.
fn gemini_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    body.get("models")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter(|m| {
                    m.get("supportedGenerationMethods")
                        .and_then(|g| g.as_array())
                        .is_none_or(|g| g.iter().any(|x| x == "generateContent"))
                })
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?;
                    let id = name.strip_prefix("models/").unwrap_or(name).to_owned();
                    Some(DiscoveredModel {
                        display_name: m
                            .get("displayName")
                            .and_then(|d| d.as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| id.clone()),
                        context_window: m.get("inputTokenLimit").and_then(|n| n.as_u64()).filter(|n| *n > 0),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `GET {base}/models` — works for OpenAI-compatible servers, Anthropic and Gemini.
pub async fn discover_models(
    kind: &ProviderKind,
    base_url: &str,
    key: Option<&str>,
    auth: Auth,
) -> Result<Vec<DiscoveredModel>> {
    let mut url = format!("{}/models", normalize_base_url(base_url));
    if *kind == ProviderKind::Gemini {
        url.push_str("?pageSize=1000");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut req = client.get(&url);
    match (kind, auth, key) {
        (ProviderKind::Anthropic, Auth::ClaudeLogin, Some(k)) => {
            req = req
                .bearer_auth(k)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", ANTHROPIC_OAUTH_BETA);
        }
        (ProviderKind::Anthropic, Auth::ClaudeLogin, None) => {
            return Err(anyhow!("Claude Code login required to discover models"));
        }
        (ProviderKind::Anthropic, Auth::Key, Some(k)) => {
            req = req
                .header("x-api-key", k)
                .header("anthropic-version", ANTHROPIC_VERSION);
        }
        (ProviderKind::Anthropic, Auth::Key, None) => {
            req = req.header("anthropic-version", ANTHROPIC_VERSION);
        }
        (ProviderKind::Gemini, _, Some(k)) => req = req.header("x-goog-api-key", k),
        (_, _, Some(k)) => req = req.bearer_auth(k),
        (_, _, None) => {}
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GET {url} returned {status}: {}", truncate(&body, 400)));
    }
    if *kind == ProviderKind::Gemini {
        let body: serde_json::Value = resp.json().await.context("parse model list")?;
        let mut out = gemini_models(&body);
        out.sort_by(|a, b| a.id.cmp(&b.id));
        return Ok(out);
    }
    let list: ModelList = resp.json().await.context("parse model list")?;
    let mut out: Vec<DiscoveredModel> = list
        .data
        .into_iter()
        .map(|m| DiscoveredModel {
            display_name: m.display_name.unwrap_or_else(|| m.id.clone()),
            context_window: context_window_of(&m.rest),
            id: m.id,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::context_window_of;

    #[test]
    fn reads_context_window_variants() {
        let m = |j: serde_json::Value| j.as_object().unwrap().clone();
        assert_eq!(context_window_of(&m(serde_json::json!({"context_length": 131072}))), Some(131072));
        assert_eq!(context_window_of(&m(serde_json::json!({"max_model_len": "32768"}))), Some(32768));
        assert_eq!(
            context_window_of(&m(serde_json::json!({"top_provider": {"context_length": 200000}}))),
            Some(200000)
        );
        assert_eq!(context_window_of(&m(serde_json::json!({"owned_by": "x"}))), None);
    }

    #[test]
    fn lists_gemini_models() {
        let body = serde_json::json!({"models": [
            {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro", "inputTokenLimit": 1048576,
             "supportedGenerationMethods": ["generateContent", "countTokens"]},
            {"name": "models/embedding-001", "supportedGenerationMethods": ["embedContent"]},
        ]});
        let m = super::gemini_models(&body);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].id.as_str(), m[0].context_window), ("gemini-2.5-pro", Some(1048576)));
    }

    #[test]
    fn claude_login_leads_with_claude_code_identity() {
        use super::*;
        let resolved = |auth| Resolved {
            provider: Provider {
                id: "p".into(),
                name: "Anthropic".into(),
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com/v1".into(),
                has_key: false,
                auth_mode: AuthMode::ClaudeLogin,
            },
            model: Model {
                id: "m".into(),
                provider_id: "p".into(),
                model_id: "claude-opus-4-8".into(),
                display_name: "Claude Opus 4.8".into(),
                vision: true,
                context_window: None,
                params: Default::default(),
            },
            key: None,
            auth,
        };
        let request = || patchbay_llm::Request {
            system: vec![SystemBlock::new("be terse")],
            messages: vec![patchbay_llm::Message::user("hi")],
            ..Default::default()
        };
        let systems = |req: &patchbay_llm::Request| -> Vec<String> {
            req.system.iter().map(|b| b.text.clone()).collect()
        };

        let mut req = request();
        prepare_request(&resolved(Auth::ClaudeLogin), &mut req);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(systems(&req), [CLAUDE_CODE_IDENTITY, "be terse"]);

        let mut req = request();
        prepare_request(&resolved(Auth::Key), &mut req);
        assert_eq!(systems(&req), ["be terse"]);
    }
}
