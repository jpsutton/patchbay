//! Patchbay's own coding agent (`crates/patchbay-agent`), run inside the app.
//!
//! The agent speaks ACP over an in-memory pipe, so `acp.rs` drives it like any other agent. This
//! module is its host: it gives the agent Patchbay's models, keys and settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use patchbay_agent::{ClaudeConfig, Host, Limits, ModelClient, ModelInfo, Settings};

use super::acp::NativeAgent;
use super::grok_config::{model_key, model_ref_from_key};
use crate::db::Db;
use crate::providers;

pub struct AppHost {
    db: Arc<Db>,
    data_dir: PathBuf,
}

/// The factory `acp::spawn` calls to start the agent on its pipe.
pub fn launcher(db: Arc<Db>, data_dir: &Path) -> NativeAgent {
    let host = Arc::new(AppHost { db, data_dir: data_dir.to_path_buf() });
    Arc::new(move |reader, writer| {
        let host = host.clone();
        Box::pin(patchbay_agent::serve(host, reader, writer))
    })
}

/// Picker labels for `models`. A display name that more than one model shares gets its provider's
/// name in parentheses, so the same model from two providers can be told apart.
fn picker_names<'a>(models: &[crate::db::Model], provider_name: impl Fn(&str) -> Option<&'a str>) -> Vec<String> {
    models
        .iter()
        .map(|m| {
            let shared = models.iter().filter(|o| o.display_name == m.display_name).count() > 1;
            match provider_name(&m.provider_id) {
                Some(p) if shared => format!("{} ({p})", m.display_name),
                _ => m.display_name.clone(),
            }
        })
        .collect()
}

#[async_trait]
impl Host for AppHost {
    fn models(&self) -> Vec<ModelInfo> {
        let models = self.db.list_models().unwrap_or_default();
        let providers = self.db.list_providers().unwrap_or_default();
        let names = picker_names(
            &models,
            |id| providers.iter().find(|(p, _)| p.id == id).map(|(p, _)| p.name.as_str()),
        );
        models
            .into_iter()
            .zip(names)
            .map(|(m, name)| ModelInfo { id: model_key(&m.id), effort: m.params.reasoning_effort(), name })
            .collect()
    }

    async fn model_client(&self, model_id: &str) -> Result<ModelClient, String> {
        let model_ref = model_ref_from_key(model_id).ok_or_else(|| format!("unknown model {model_id}"))?;
        let mut resolved = providers::resolve(&self.db, model_ref).map_err(|e| format!("{e:#}"))?;
        providers::ensure_token(&mut resolved).await.map_err(|e| format!("{e:#}"))?;
        let prefs = crate::prefs::load(&self.db);
        let engine = prefs.agent_engine;
        // The plan's usage limit (Claude or Grok) only guards a turn while "Stop at plan limit" is on.
        let plan_guarded = matches!(resolved.auth, providers::Auth::ClaudeLogin | providers::Auth::GrokLogin) && prefs.usage_limit.enabled;
        let mut cfg = providers::llm_config(&resolved);
        cfg.cache_ttl = providers::agent_cache_ttl(&resolved);
        if let Some(n) = engine.request_retries {
            cfg.max_retries = n.min(10);
        }
        if let Some(s) = engine.idle_timeout_secs.filter(|s| *s > 0) {
            cfg.idle_timeout = std::time::Duration::from_secs(s.into());
        }
        Ok(ModelClient {
            client: patchbay_llm::Client::new(cfg),
            system_prefix: providers::system_prefix(&resolved),
            context_window: resolved.model.context_window.and_then(|c| u64::try_from(c).ok()),
            auto_compact_percent: resolved.model.params.auto_compact_percent,
            plan_guarded,
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn web_search_key(&self) -> Option<String> {
        providers::web_search_key(&self.db).map(|(k, _)| k)
    }

    fn native_web_search(&self, model_id: &str) -> bool {
        model_ref_from_key(model_id)
            .and_then(|r| providers::resolve(&self.db, r).ok())
            .is_some_and(|r| providers::native_web_search(&r))
    }

    fn settings(&self) -> Settings {
        let prefs = crate::prefs::load(&self.db);
        let (e, d, c) = (prefs.agent_engine, Limits::default(), prefs.claude_config.clone());
        Settings {
            web_fetch: prefs.web_fetch,
            minify_prompts: prefs.minify_prompts,
            auto_compact_percent: prefs.auto_compact_percent.unwrap_or(0),
            instructions: prefs.agent_instructions,
            limits: Limits {
                max_steps: e.max_steps.map(|n| n.clamp(1, 10_000) as usize),
                subagent_max_steps: e.subagent_max_steps.map_or(d.subagent_max_steps, |n| n.clamp(1, 10_000) as usize),
                shell_timeout_ms: e.shell_timeout_secs.map_or(d.shell_timeout_ms, |s| u64::from(s.clamp(1, 600)) * 1000),
            },
            claude: ClaudeConfig {
                skills: c.skills,
                instructions: c.instructions,
                agents: c.agents,
                mcp_servers: c.mcp_servers,
                hooks: c.hooks,
                plugins: c.plugins,
                disabled_plugins: c.disabled_plugins,
                disabled_mcp_servers: c.disabled_mcp_servers,
            },
        }
    }

    fn read_pdf(&self, path: &Path, pages: Option<&str>) -> Result<String, String> {
        crate::attachments::pdf_text(path, pages).map_err(|e| format!("{e:#}"))
    }

    fn prepare_image(&self, bytes: &[u8], mime: &str) -> Result<(String, String), String> {
        match crate::attachments::prepare_image(bytes, mime).map_err(|e| format!("{e:#}"))? {
            crate::attachments::Prepared::Image { mime, b64 } => Ok((mime, b64)),
            crate::attachments::Prepared::Text { .. } => Err("not an image".into()),
        }
    }
}

/// What Patchbay's agent finds in the user's Claude Code setup, for Settings: everything there,
/// whatever the switches say, with what's turned off marked. `cwd` adds a project's own setup.
pub fn claude_overview(prefs: &crate::prefs::Preferences, cwd: Option<&Path>) -> serde_json::Value {
    use patchbay_agent::claude;
    use serde_json::json;
    let home = claude::home();
    let home = home.as_deref().filter(|h| h.is_dir());
    let c = &prefs.claude_config;
    // Discover with every part on; the UI shows the switches beside what they'd load.
    let all = ClaudeConfig::default();
    let cwd = cwd.filter(|p| p.is_dir());
    let project = cwd.map(Path::to_path_buf).unwrap_or_else(|| home.and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default());
    let roots = claude::roots(home, &project, &all);
    let plugins: Vec<_> = home.map(claude::plugins).unwrap_or_default();
    let item = |name: &str, description: &str, source: &claude::Source| json!({"name": name, "description": description, "source": source.label()});
    json!({
        "home": home,
        "project": cwd,
        "instructions": claude::instructions(home, &project, &all).into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
        "plugins": plugins.iter().map(|p| json!({
            "id": p.id,
            "name": p.name,
            "enabledInClaude": p.enabled,
            "disabled": c.disabled_plugins.contains(&p.id),
        })).collect::<Vec<_>>(),
        "commands": claude::commands(&roots).iter().map(|x| item(&x.name, &x.description, &x.source)).collect::<Vec<_>>(),
        "skills": claude::skills(&roots).iter().map(|x| item(&x.name, &x.description, &x.source)).collect::<Vec<_>>(),
        "agents": claude::agents(&roots).iter().map(|x| item(&x.name, &x.description, &x.source)).collect::<Vec<_>>(),
        "mcpServers": claude::mcp_servers(home, &project, &roots).iter().map(|e| json!({
            "name": e.name,
            "source": e.source.label(),
            "transport": e.transport,
            "disabled": c.disabled_mcp_servers.contains(&e.name),
        })).collect::<Vec<_>>(),
        "hooks": claude::hooks(home, &project, &roots).iter().map(|h| json!({
            "event": h.event,
            "matcher": h.matcher,
            "command": h.command,
            "source": h.source.label(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, provider_id: &str, display_name: &str) -> crate::db::Model {
        crate::db::Model {
            id: id.into(),
            provider_id: provider_id.into(),
            model_id: id.into(),
            display_name: display_name.into(),
            vision: false,
            context_window: None,
            params: Default::default(),
        }
    }

    #[test]
    fn picker_names_tag_only_shared_names() {
        let models = [model("a", "p1", "Claude Opus 5.5"), model("b", "p2", "Claude Opus 5.5"), model("c", "p1", "Claude Mythos 5.1")];
        let provider = |id: &str| match id {
            "p1" => Some("Claude Plan"),
            "p2" => Some("Anthropic CVP"),
            _ => None,
        };
        assert_eq!(
            picker_names(&models, provider),
            ["Claude Opus 5.5 (Claude Plan)", "Claude Opus 5.5 (Anthropic CVP)", "Claude Mythos 5.1"]
        );
    }
}
