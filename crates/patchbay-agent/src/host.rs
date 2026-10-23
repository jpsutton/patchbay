//! What the embedding app provides.

use std::path::PathBuf;

use async_trait::async_trait;
use patchbay_llm::{Client, Effort, SystemBlock};

#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    /// The id ACP shows and `session/set_model` takes.
    pub id: String,
    pub name: String,
    /// The reasoning effort the model is set up with, shown as the session's "model default".
    pub effort: Option<Effort>,
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
    pub limits: Limits,
    /// What to load from the user's Claude Code setup.
    pub claude: ClaudeConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            web_fetch: true,
            auto_compact_percent: 0,
            instructions: String::new(),
            minify_prompts: false,
            limits: Limits::default(),
            claude: ClaudeConfig::default(),
        }
    }
}

/// How far the agent may go on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Model requests one prompt may make.
    pub max_steps: usize,
    /// Model requests one subagent run may make.
    pub subagent_max_steps: usize,
    /// Shell commands time out after this long unless the model asks for another timeout.
    pub shell_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_steps: 100, subagent_max_steps: 40, shell_timeout_ms: 120_000 }
    }
}

/// Which parts of the user's Claude Code setup (`~/.claude`, a project's `.claude/`) to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeConfig {
    /// Skills and slash commands.
    pub skills: bool,
    /// CLAUDE.md files and `.claude/rules`.
    pub instructions: bool,
    /// Subagent definitions (`.claude/agents`).
    pub agents: bool,
    /// MCP servers from Claude's config.
    pub mcp_servers: bool,
    /// Hooks from Claude's settings; these run commands.
    pub hooks: bool,
    /// Installed Claude Code plugins, with what they bundle.
    pub plugins: bool,
    /// Plugins (`name@marketplace`) not to load, whatever Claude Code's own setting.
    pub disabled_plugins: Vec<String>,
    /// MCP servers from Claude's config not to start, by name.
    pub disabled_mcp_servers: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        ClaudeConfig {
            skills: true,
            instructions: true,
            agents: true,
            mcp_servers: true,
            hooks: true,
            plugins: true,
            disabled_plugins: Vec::new(),
            disabled_mcp_servers: Vec::new(),
        }
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
