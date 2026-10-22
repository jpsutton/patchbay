//! What the embedding app provides.

use std::path::PathBuf;

use async_trait::async_trait;
use patchbay_llm::{Client, SystemBlock};

#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// The id ACP shows and `session/set_model` takes.
    pub id: String,
    pub name: String,
}

/// A model ready to call.
pub struct ModelClient {
    pub client: Client,
    /// Blocks the provider needs ahead of the agent's own (Claude subscription logins).
    pub system_prefix: Vec<SystemBlock>,
    pub context_window: Option<u64>,
    /// Compact when the context reaches this percentage of the window; `None` = the host default.
    pub auto_compact_percent: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// WebFetch is offered.
    pub web_fetch: bool,
    /// Compact the conversation when the context reaches this percentage (0 = never).
    pub auto_compact_percent: u8,
    /// The user's own instructions, appended to the system prompt.
    pub instructions: String,
    /// Send the agent's own prompts in their minified form (fewer input tokens). A session can
    /// override this.
    pub minify_prompts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { web_fetch: true, auto_compact_percent: 0, instructions: String::new(), minify_prompts: false }
    }
}

#[async_trait]
pub trait Host: Send + Sync + 'static {
    fn models(&self) -> Vec<ModelInfo>;

    /// The model a new session starts with.
    fn default_model(&self) -> Option<String> {
        self.models().into_iter().next().map(|m| m.id)
    }

    /// Resolve `model_id` to an endpoint and credentials. Called for every request, so tokens
    /// that expire can be refreshed.
    async fn model_client(&self, model_id: &str) -> Result<ModelClient, String>;

    /// Where sessions are stored.
    fn data_dir(&self) -> PathBuf;

    /// The Brave Search API key; WebSearch is offered when there is one.
    fn web_search_key(&self) -> Option<String> {
        None
    }

    fn settings(&self) -> Settings {
        Settings::default()
    }

    /// Text of a PDF (`pages` like "1-5"), for the Read tool.
    fn read_pdf(&self, _path: &std::path::Path, _pages: Option<&str>) -> Result<String, String> {
        Err("reading PDFs is not available in this build".into())
    }
}
