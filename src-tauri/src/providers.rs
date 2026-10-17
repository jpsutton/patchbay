//! Provider configuration: secrets, model discovery, mapping to Grok Build's `SamplerConfig`.

use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_grok_sampler::{ApiBackend, AuthScheme, SamplerConfig};

use crate::db::{Db, Model, Provider, ProviderKind};

const KEYRING_SERVICE: &str = "dev.patchbay.app";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

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

pub fn backend_for(kind: &ProviderKind) -> ApiBackend {
    match kind {
        ProviderKind::OpenaiChat => ApiBackend::ChatCompletions,
        ProviderKind::OpenaiResponses => ApiBackend::Responses,
        ProviderKind::Anthropic => ApiBackend::Messages,
    }
}

/// Resolved model + provider + key, ready to sample.
pub struct Resolved {
    pub provider: Provider,
    pub model: Model,
    pub key: Option<String>,
}

pub fn resolve(db: &Db, model_ref: &str) -> Result<Resolved> {
    let model = db
        .get_model(model_ref)?
        .ok_or_else(|| anyhow!("model {model_ref} not found; pick a model in Settings"))?;
    let (provider, inline) = db
        .get_provider(&model.provider_id)?
        .ok_or_else(|| anyhow!("provider for model {} missing", model.display_name))?;
    let key = load_key(&provider.id, inline.as_deref());
    Ok(Resolved {
        provider,
        model,
        key,
    })
}

pub fn sampler_config(r: &Resolved) -> SamplerConfig {
    let mut extra_headers = IndexMap::new();
    let mut auth_scheme = AuthScheme::Bearer;
    if r.provider.kind == ProviderKind::Anthropic {
        extra_headers.insert("anthropic-version".into(), ANTHROPIC_VERSION.into());
        auth_scheme = AuthScheme::XApiKey;
    }
    SamplerConfig {
        api_key: r.key.clone(),
        base_url: normalize_base_url(&r.provider.base_url),
        model: r.model.model_id.clone(),
        api_backend: backend_for(&r.provider.kind),
        auth_scheme,
        extra_headers,
        context_window: r.model.context_window.unwrap_or(200_000) as u64,
        // Anthropic requires max_tokens; default it when the user hasn't set one.
        max_completion_tokens: r
            .model
            .params
            .max_output_tokens
            .or((r.provider.kind == ProviderKind::Anthropic).then_some(8192)),
        temperature: r.model.params.temperature,
        top_p: r.model.params.top_p,
        reasoning_effort: r.model.params.reasoning_effort(),
        reasoning_summary: r.model.params.reasoning_summary(),
        idle_timeout_secs: Some(180),
        max_retries: Some(2),
        client_identifier: Some("patchbay".into()),
        client_version: Some(env!("CARGO_PKG_VERSION").into()),
        ..Default::default()
    }
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

/// `GET {base}/models` — works for OpenAI-compatible servers and the Anthropic API.
pub async fn discover_models(
    kind: &ProviderKind,
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<DiscoveredModel>> {
    let url = format!("{}/models", normalize_base_url(base_url));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut req = client.get(&url);
    match (kind, key) {
        (ProviderKind::Anthropic, Some(k)) => {
            req = req
                .header("x-api-key", k)
                .header("anthropic-version", ANTHROPIC_VERSION);
        }
        (ProviderKind::Anthropic, None) => {
            req = req.header("anthropic-version", ANTHROPIC_VERSION);
        }
        (_, Some(k)) => req = req.bearer_auth(k),
        (_, None) => {}
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GET {url} returned {status}: {}", truncate(&body, 400)));
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
}
