//! Patchbay's providers for agents that bring their own sign-in (Claude Code, OpenCode, Kilo),
//! when Settings → Coding agents turns it on for one. Off by default: the agent keeps its setup.
//!
//! Keys never reach a config file or the command line. OpenCode and Kilo read them from env
//! vars their inline config names (`{env:PATCHBAY_KEY_<n>}`); Claude Code reads its env.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::agents::AgentKind;
use crate::db::{Db, ProviderKind};
use crate::providers;

/// What an agent's launch gains from a Patchbay provider.
#[derive(Debug, Default)]
pub struct Setup {
    /// Inline config to merge into OpenCode's or Kilo's (`*_CONFIG_CONTENT`).
    pub config: Map<String, Value>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
}

/// Agents that can run on a Patchbay provider instead of their own sign-in.
pub fn supported(kind: AgentKind) -> bool {
    matches!(kind, AgentKind::ClaudeCode | AgentKind::Opencode | AgentKind::Kilo)
}

/// Provider kinds an agent can use: Claude Code speaks only the Anthropic Messages API.
pub fn takes_kind(agent: AgentKind, kind: &ProviderKind) -> bool {
    agent != AgentKind::ClaudeCode || *kind == ProviderKind::Anthropic
}

/// OpenCode's id for a Patchbay provider. Kept stable, so a session's picked model survives
/// providers being added or removed.
pub fn opencode_provider_id(provider_id: &str) -> String {
    let slug: String = provider_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("patchbay-{slug}")
}

/// The setup for `kind` running on Patchbay model `model_ref`.
pub fn setup(db: &Db, kind: AgentKind, model_ref: &str) -> Result<Setup> {
    match kind {
        AgentKind::ClaudeCode => claude_code(db, model_ref),
        AgentKind::Opencode | AgentKind::Kilo => opencode(db, model_ref),
        _ => bail!("{} can't use Patchbay's providers", kind.label()),
    }
}

/// Claude Code against an Anthropic-compatible endpoint. Every model it would pick (its default,
/// the opus/sonnet/haiku aliases, background tasks, subagents) is the chosen one.
fn claude_code(db: &Db, model_ref: &str) -> Result<Setup> {
    let r = providers::resolve(db, model_ref)?;
    if !takes_kind(AgentKind::ClaudeCode, &r.provider.kind) {
        bail!(
            "Claude Code needs an Anthropic-compatible provider, and {} isn't one. Pick another model in Settings → Coding agents → Claude Code.",
            r.provider.name
        );
    }
    // Claude Code adds `/v1/messages` itself.
    let base = providers::normalize_base_url(&r.provider.base_url);
    let base = base.strip_suffix("/v1").unwrap_or(&base).to_owned();
    let model = r.model.model_id;
    let mut env = vec![("ANTHROPIC_BASE_URL".to_owned(), base)];
    if let Some(key) = r.key {
        env.push(("ANTHROPIC_API_KEY".to_owned(), key));
    }
    for var in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        env.push((var.to_owned(), model.clone()));
    }
    // A bearer token from the user's environment would win over the key.
    Ok(Setup { config: Map::new(), env, env_remove: vec!["ANTHROPIC_AUTH_TOKEN".into()] })
}

/// Every Patchbay provider and model as OpenCode providers, with `model_ref` the default.
/// Written in the original config schema, which OpenCode 2 still reads and Kilo uses.
fn opencode(db: &Db, model_ref: &str) -> Result<Setup> {
    let default = db.get_model(model_ref)?.ok_or_else(|| anyhow!("model {model_ref} not found; pick one in Settings → Coding agents"))?;
    let models = db.list_models()?;
    let mut provider = Map::new();
    let mut env = Vec::new();
    for (i, (p, inline)) in db.list_providers()?.into_iter().enumerate() {
        let mut entries = Map::new();
        for m in models.iter().filter(|m| m.provider_id == p.id) {
            let mut e = json!({ "name": m.display_name });
            if let (Some(context), Some(output)) = (m.context_window, m.params.max_output_tokens) {
                e["limit"] = json!({ "context": context, "output": output });
            }
            entries.insert(m.model_id.clone(), e);
        }
        if entries.is_empty() {
            continue;
        }
        let npm = match p.kind {
            ProviderKind::OpenaiChat => "@ai-sdk/openai-compatible",
            ProviderKind::OpenaiResponses => "@ai-sdk/openai",
            ProviderKind::Anthropic => "@ai-sdk/anthropic",
        };
        let mut options = json!({ "baseURL": providers::normalize_base_url(&p.base_url) });
        if let Some(key) = providers::load_key(&p.id, inline.as_deref()) {
            let var = format!("PATCHBAY_KEY_{i}");
            options["apiKey"] = json!(format!("{{env:{var}}}"));
            env.push((var, key));
        }
        provider.insert(
            opencode_provider_id(&p.id),
            json!({ "npm": npm, "name": format!("{} (Patchbay)", p.name), "options": options, "models": entries }),
        );
    }
    let mut config = Map::new();
    config.insert("provider".into(), Value::Object(provider));
    config.insert(
        "model".into(),
        json!(format!("{}/{}", opencode_provider_id(&default.provider_id), default.model_id)),
    );
    Ok(Setup { config, env, env_remove: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Model, ModelParams, Provider};

    fn db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pb-ap-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        for (id, kind, url) in [
            ("fw", ProviderKind::OpenaiChat, "https://api.fireworks.ai/inference/v1/"),
            ("ant", ProviderKind::Anthropic, "https://api.anthropic.com/v1"),
        ] {
            let p = Provider { id: id.into(), name: id.to_uppercase(), kind, base_url: url.into(), has_key: true, auth_mode: Default::default(), };
            db.upsert_provider(&p, Some(&format!("key-{id}"))).unwrap();
        }
        for (id, provider, model, cw, out) in [
            ("glm", "fw", "accounts/fireworks/models/glm-5", Some(200_000), Some(16_384)),
            ("son", "ant", "claude-sonnet-5-5", None, None),
        ] {
            db.upsert_model(&Model {
                id: id.into(),
                provider_id: provider.into(),
                model_id: model.into(),
                display_name: model.into(),
                vision: false,
                context_window: cw,
                params: ModelParams { max_output_tokens: out, ..Default::default() },
            })
            .unwrap();
        }
        (db, dir)
    }

    #[test]
    fn opencode_gets_every_provider_with_keys_by_env() {
        let (db, dir) = db();
        let s = setup(&db, AgentKind::Opencode, "glm").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let fw = &s.config["provider"]["patchbay-fw"];
        assert_eq!(fw["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(fw["options"]["baseURL"], "https://api.fireworks.ai/inference/v1");
        let var = fw["options"]["apiKey"].as_str().unwrap().trim_start_matches("{env:").trim_end_matches('}');
        assert!(s.env.iter().any(|(k, v)| k == var && v == "key-fw"));
        assert_eq!(fw["models"]["accounts/fireworks/models/glm-5"]["limit"], json!({ "context": 200000, "output": 16384 }));
        assert_eq!(s.config["provider"]["patchbay-ant"]["npm"], "@ai-sdk/anthropic");
        assert!(s.config["provider"]["patchbay-ant"]["models"]["claude-sonnet-5-5"].get("limit").is_none());
        assert_eq!(s.config["model"], "patchbay-fw/accounts/fireworks/models/glm-5");
        assert!(!serde_json::to_string(&s.config).unwrap().contains("key-"), "no key in the config");
    }

    #[test]
    fn claude_code_needs_an_anthropic_provider() {
        let (db, dir) = db();
        let err = setup(&db, AgentKind::ClaudeCode, "glm").unwrap_err();
        let s = setup(&db, AgentKind::ClaudeCode, "son").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(format!("{err:#}").contains("Anthropic-compatible"), "{err:#}");
        let env: std::collections::HashMap<_, _> = s.env.into_iter().collect();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.anthropic.com");
        assert_eq!(env["ANTHROPIC_API_KEY"], "key-ant");
        assert_eq!(env["ANTHROPIC_MODEL"], "claude-sonnet-5-5");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-sonnet-5-5");
        assert_eq!(s.env_remove, vec!["ANTHROPIC_AUTH_TOKEN".to_owned()]);
    }

    #[test]
    fn provider_ids_stay_valid_for_opencode() {
        assert_eq!(opencode_provider_id("a/b#c d"), "patchbay-a-b-c-d");
    }
}
