//! Patchbay's own coding agent (`crates/patchbay-agent`), run inside the app.
//!
//! The agent speaks ACP over an in-memory pipe, so `acp.rs` drives it like any other agent. This
//! module is its host: it gives the agent Patchbay's models, keys and settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use patchbay_agent::{Host, ModelClient, ModelInfo, Settings};

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

#[async_trait]
impl Host for AppHost {
    fn models(&self) -> Vec<ModelInfo> {
        self.db
            .list_models()
            .unwrap_or_default()
            .into_iter()
            .map(|m| ModelInfo { id: model_key(&m.id), name: m.display_name })
            .collect()
    }

    async fn model_client(&self, model_id: &str) -> Result<ModelClient, String> {
        let model_ref = model_ref_from_key(model_id).ok_or_else(|| format!("unknown model {model_id}"))?;
        let mut resolved = providers::resolve(&self.db, model_ref).map_err(|e| format!("{e:#}"))?;
        providers::ensure_token(&mut resolved).await.map_err(|e| format!("{e:#}"))?;
        Ok(ModelClient {
            client: patchbay_llm::Client::new(providers::llm_config(&resolved)),
            system_prefix: providers::system_prefix(&resolved),
            context_window: resolved.model.context_window.and_then(|c| u64::try_from(c).ok()),
            auto_compact_percent: resolved.model.params.auto_compact_percent,
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn web_search_key(&self) -> Option<String> {
        providers::web_search_key(&self.db).map(|(k, _)| k)
    }

    fn settings(&self) -> Settings {
        let prefs = crate::prefs::load(&self.db);
        Settings {
            web_fetch: prefs.web_fetch,
            auto_compact_percent: prefs.auto_compact_percent.unwrap_or(0),
            instructions: prefs.agent_instructions,
        }
    }

    fn read_pdf(&self, path: &Path, pages: Option<&str>) -> Result<String, String> {
        crate::attachments::pdf_text(path, pages).map_err(|e| format!("{e:#}"))
    }
}
