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
mod host;
mod mcp;
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
pub use agent::serve;
pub use store::sessions_dir;
pub use tools::web::{brave_search, web_search_spec};
pub use host::{ClaudeConfig, Host, Limits, ModelClient, ModelInfo, Settings};
