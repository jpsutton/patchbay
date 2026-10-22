//! `patchbay-agent`: the agent on stdio, for ACP probes and tests.
//!
//! Models come from the JSON file named by `PATCHBAY_AGENT_MODELS`:
//! `[{"id": "m", "name": "M", "format": "chat_completions|responses|anthropic|gemini",
//!    "base_url": "...", "model": "...", "key": "..."}]`. Sessions are stored under
//! `PATCHBAY_AGENT_DATA` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use patchbay_agent::{Host, ModelClient, ModelInfo, Settings};
use patchbay_llm::{Client, Config, WireFormat};
use serde::Deserialize;

#[derive(Deserialize, Clone)]
struct Entry {
    id: String,
    name: Option<String>,
    format: WireFormat,
    base_url: String,
    model: String,
    key: Option<String>,
}

struct FileHost {
    models: Vec<Entry>,
    data_dir: PathBuf,
}

#[async_trait]
impl Host for FileHost {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.iter().map(|m| ModelInfo { id: m.id.clone(), name: m.name.clone().unwrap_or_else(|| m.id.clone()) }).collect()
    }

    async fn model_client(&self, model_id: &str) -> Result<ModelClient, String> {
        let m = self.models.iter().find(|m| m.id == model_id).ok_or_else(|| format!("unknown model {model_id}"))?;
        let mut cfg = Config::new(m.format, &m.base_url, &m.model);
        cfg.key = m.key.clone();
        Ok(ModelClient { client: Client::new(cfg), system_prefix: Vec::new(), context_window: None })
    }

    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn settings(&self) -> Settings {
        Settings::default()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let models = std::env::var("PATCHBAY_AGENT_MODELS")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let data_dir = std::env::var_os("PATCHBAY_AGENT_DATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let host = Arc::new(FileHost { models, data_dir });
    patchbay_agent::serve(host, tokio::io::stdin(), tokio::io::stdout()).await;
}
