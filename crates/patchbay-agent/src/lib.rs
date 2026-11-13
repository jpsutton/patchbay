//! Patchbay's own coding agent.
//!
//! An ACP server (JSON-RPC over any async byte stream) that runs the model loop itself: it
//! builds the prompt, streams a reply through [`patchbay_llm`], asks the client for permission,
//! runs file, search, shell, web and MCP tools, and persists the conversation. The host app
//! supplies providers and keys through [`Host`]; nothing about providers lives here.

mod agent;
mod cache;
mod compact;
mod extract;
mod history;
mod host;
mod mcp;
pub use mcp::{McpChildEvent, McpChildInfo, McpObserver, McpSet, set_observer};
mod prompt;
mod prompts;
mod reqlog;
mod rpc;
mod session;
mod store;
mod subagent;
mod tools;
mod turn;

pub mod claude;
pub mod preview;
pub mod proc;
pub use agent::serve;
pub use history::{HistoryWindow, history_window};
pub use store::sessions_dir;
pub use tools::{foreign_location, foreign_target};
pub use tools::web::{brave_search, web_search_spec};
pub use host::{ClaudeConfig, DEFAULT_MAX_STEPS, Host, Limits, ModelClient, ModelInfo, Settings};
