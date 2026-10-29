//! Render a private `$GROK_HOME/config.toml` for an external Grok Build from Patchbay's providers.
//!
//! API keys never touch this file: each provider key is passed to the child as an env var
//! (`PATCHBAY_KEY_<n>`) referenced by `env_key` (Bearer) or `env_http_headers` (x-api-key).

use std::path::Path;

use anyhow::Result;
use toml::{Table, Value};

use crate::db::{Db, ProviderKind};
use crate::providers::{self, ANTHROPIC_VERSION};

pub struct Rendered {
    pub toml: String,
    /// (env var name, secret) pairs to set on the child process.
    pub env: Vec<(String, String)>,
}

pub fn model_key(model_ref: &str) -> String {
    format!("pb-{model_ref}")
}

pub fn model_ref_from_key(key: &str) -> Option<&str> {
    key.strip_prefix("pb-")
}

fn table(pairs: &[(&str, Value)]) -> Value {
    Value::Table(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

pub fn render(db: &Db, default_model_ref: Option<&str>) -> Result<Rendered> {
    let mut root = Table::new();
    let mut env = Vec::new();
    let prefs = crate::prefs::load(db);

    // No product telemetry, no remote catalog/asset fetches, no auto-update, no leader daemon.
    root.insert(
        "features".into(),
        table(&[
            ("telemetry", Value::Boolean(false)),
            ("remote_fetch", Value::Boolean(false)),
            ("non_git_warning", Value::Boolean(false)),
            ("web_fetch", Value::Boolean(prefs.web_fetch)),
            ("session_recap", Value::Boolean(prefs.background_summaries)),
            ("title_refresh", Value::Boolean(prefs.background_summaries)),
        ]),
    );
    // Grok always asks, and Patchbay answers from each session's approval mode
    // (code/permissions.rs). This file is shared by every session, so it can't hold theirs.
    root.insert("ui".into(), table(&[("permission_mode", s("default"))]));
    if let Some(p) = prefs.auto_compact_percent {
        root.insert(
            "session".into(),
            table(&[("auto_compact_threshold_percent", Value::Integer(p.into()))]),
        );
    }
    root.insert(
        "cli".into(),
        table(&[
            ("auto_update", Value::Boolean(false)),
            ("use_leader", Value::Boolean(false)),
        ]),
    );
    // Grok's vendor-compat cells: what it loads from Claude Code's config.
    let cc = &prefs.claude_config;
    root.insert(
        "compat".into(),
        Value::Table(Table::from_iter([(
            "claude".to_owned(),
            table(&[
                ("skills", Value::Boolean(cc.skills)),
                ("rules", Value::Boolean(cc.instructions)),
                ("agents", Value::Boolean(cc.agents)),
                ("mcps", Value::Boolean(cc.mcp_servers)),
                ("hooks", Value::Boolean(cc.hooks)),
            ]),
        )])),
    );
    if !cc.plugins {
        // No config cell for this; only builds of Grok Build that read the env var honour it.
        env.push(("GROK_CLAUDE_PLUGINS_ENABLED".to_owned(), "false".to_owned()));
    }
    root.insert(
        "telemetry".into(),
        table(&[
            ("trace_upload", Value::Boolean(false)),
            ("otel_enabled", Value::Boolean(false)),
        ]),
    );

    let mut provider_tables = Table::new();
    for (i, (p, inline)) in db.list_providers()?.into_iter().enumerate() {
        let mut t = Table::new();
        let mut base_url = providers::normalize_base_url(&p.base_url);
        if p.kind == ProviderKind::Gemini {
            // Grok Build has no Gemini backend; Google serves the OpenAI dialect under /openai.
            base_url.push_str("/openai");
        }
        t.insert("base_url".into(), s(&base_url));
        let backend = match p.kind {
            ProviderKind::OpenaiChat | ProviderKind::Gemini => "chat_completions",
            ProviderKind::OpenaiResponses => "responses",
            ProviderKind::Anthropic => "messages",
        };
        t.insert("api_backend".into(), s(backend));
        if let Some(key) = providers::load_key(&p.id, inline.as_deref()) {
            let var = format!("PATCHBAY_KEY_{i}");
            if p.kind == ProviderKind::Anthropic {
                t.insert("env_http_headers".into(), table(&[("x-api-key", s(&var))]));
            } else {
                t.insert("env_key".into(), s(&var));
            }
            env.push((var, key));
        }
        if p.kind == ProviderKind::Anthropic {
            t.insert(
                "extra_headers".into(),
                table(&[("anthropic-version", s(ANTHROPIC_VERSION))]),
            );
        }
        provider_tables.insert(format!("p{i}"), Value::Table(t));
    }

    let providers_by_id: Vec<String> = db
        .list_providers()?
        .into_iter()
        .map(|(p, _)| p.id)
        .collect();
    let mut model_tables = Table::new();
    let models = db.list_models()?;
    for m in &models {
        let Some(idx) = providers_by_id.iter().position(|id| *id == m.provider_id) else {
            continue;
        };
        let mut t = Table::new();
        t.insert("model".into(), s(&m.model_id));
        t.insert("name".into(), s(&m.display_name));
        t.insert("model_provider".into(), s(&format!("p{idx}")));
        if let Some(cw) = m.context_window {
            t.insert("context_window".into(), Value::Integer(cw));
        }
        let p = &m.params;
        if let Some(v) = p.max_output_tokens {
            t.insert("max_completion_tokens".into(), Value::Integer(v.into()));
        }
        if let Some(v) = p.temperature {
            t.insert("temperature".into(), Value::Float(f32_to_f64(v)));
        }
        if let Some(v) = p.top_p {
            t.insert("top_p".into(), Value::Float(f32_to_f64(v)));
        }
        if let Some(v) = p.reasoning_effort.as_deref().filter(|_| p.reasoning_effort().is_some()) {
            t.insert("reasoning_effort".into(), s(v));
        }
        if let Some(v) = p.reasoning_summary.as_deref().filter(|_| p.reasoning_summary().is_some()) {
            t.insert("reasoning_summary".into(), s(v));
        }
        if let Some(v) = p.auto_compact_percent {
            t.insert("auto_compact_threshold_percent".into(), Value::Integer(v.into()));
        }
        model_tables.insert(model_key(&m.id), Value::Table(t));
    }
    root.insert("model_providers".into(), Value::Table(provider_tables));
    root.insert("model".into(), Value::Table(model_tables));

    let default = default_model_ref
        .filter(|r| models.iter().any(|m| m.id == *r))
        .or_else(|| models.first().map(|m| m.id.as_str()));
    let mut models_section = Table::new();
    // Only Patchbay-defined models are pickable; built-in xAI catalog entries are filtered out.
    models_section.insert("allowed_models".into(), Value::Array(vec![s("pb-*")]));
    if let Some(d) = default {
        let key = model_key(d);
        // Auxiliary roles default to xAI models; point them at the session model instead.
        for k in ["default", "image_description", "session_summary", "web_search"] {
            models_section.insert(k.into(), s(&key));
        }
    }
    root.insert("models".into(), Value::Table(models_section));

    Ok(Rendered {
        toml: toml::to_string_pretty(&root)?,
        env,
    })
}

/// Widen via the shortest decimal, so 0.9f32 is written as 0.9, not 0.8999999761581421.
fn f32_to_f64(v: f32) -> f64 {
    v.to_string().parse().unwrap_or(v as f64)
}

pub fn write(grok_home: &Path, rendered: &Rendered) -> Result<()> {
    std::fs::create_dir_all(grok_home)?;
    let path = grok_home.join("config.toml");
    std::fs::write(&path, &rendered.toml)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::{Db, Model, ModelParams, Pricing, Provider, ProviderKind};

    #[test]
    fn renders_model_params() {
        let dir = std::env::temp_dir().join(format!("pb-gc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        db.upsert_provider(
            &Provider {
                id: "p".into(),
                name: "p".into(),
                kind: ProviderKind::OpenaiResponses,
                base_url: "http://x/v1".into(),
                has_key: false,
            auth_mode: Default::default(),
        },
            None,
        )
        .unwrap();
        db.upsert_model(&Model {
            id: "m".into(),
            provider_id: "p".into(),
            model_id: "gpt-x".into(),
            display_name: "X".into(),
            vision: true,
            context_window: Some(64000),
            params: ModelParams {
                max_output_tokens: Some(4096),
                temperature: Some(0.5),
                top_p: Some(0.9),
                reasoning_effort: Some("high".into()),
                reasoning_summary: Some("detailed".into()),
                auto_compact_percent: Some(70),
                send_reasoning: None,
                pricing: Pricing::default(),
            },
        })
        .unwrap();
        crate::prefs::save(
            &db,
            &crate::prefs::Preferences {
                permission_mode: "auto".into(),
                web_fetch: true,
                auto_compact_percent: Some(60),
                background_summaries: false,
                ..Default::default()
            },
        )
        .unwrap();
        let t = super::render(&db, Some("m")).unwrap().toml;
        let _ = std::fs::remove_dir_all(&dir);
        let cfg: toml::Table = toml::from_str(&t).unwrap();
        // The preference is only new sessions' default; Grok itself always asks.
        assert_eq!(cfg["ui"]["permission_mode"].as_str(), Some("default"));
        assert_eq!(cfg["features"]["web_fetch"].as_bool(), Some(true));
        assert_eq!(cfg["features"]["session_recap"].as_bool(), Some(false));
        assert_eq!(cfg["compat"]["claude"]["skills"].as_bool(), Some(true));
        assert_eq!(cfg["compat"]["claude"]["hooks"].as_bool(), Some(true));
        assert_eq!(cfg["session"]["auto_compact_threshold_percent"].as_integer(), Some(60));
        for want in [
            "max_completion_tokens = 4096",
            "temperature = 0.5",
            "top_p = 0.9",
            "reasoning_effort = \"high\"",
            "reasoning_summary = \"detailed\"",
            "auto_compact_threshold_percent = 70",
        ] {
            assert!(t.contains(want), "missing {want} in\n{t}");
        }
    }

    #[test]
    fn rejects_bad_params() {
        let bad = |f: fn(&mut ModelParams)| {
            let mut p = ModelParams::default();
            f(&mut p);
            p.validate().is_err()
        };
        assert!(bad(|p| p.temperature = Some(3.0)));
        assert!(bad(|p| p.top_p = Some(1.5)));
        assert!(bad(|p| p.reasoning_effort = Some("extreme".into())));
        assert!(bad(|p| p.pricing.input = Some(-1.0)));
        assert!(ModelParams::default().validate().is_ok());
    }
}
