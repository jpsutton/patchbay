//! Patchbay's own coding agent.
//!
//! An ACP server (JSON-RPC over any async byte stream) that runs the model loop itself: it
//! builds the prompt, streams a reply through [`patchbay_llm`], asks the client for permission,
//! runs file, search, shell, web and MCP tools, and persists the conversation. The host app
//! supplies providers and keys through [`Host`]; nothing about providers lives here.

mod agent;
mod compact;
mod host;
mod mcp;
mod prompt;
mod rpc;
mod session;
mod store;
mod tools;
mod turn;

pub use agent::serve;
pub use store::sessions_dir;
pub use host::{Host, ModelClient, ModelInfo, Settings};
