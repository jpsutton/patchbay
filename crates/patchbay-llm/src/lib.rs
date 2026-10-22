//! Patchbay's LLM client: one provider-neutral conversation model, four wire formats.
//!
//! Callers build a [`Request`] from [`Message`]s and [`Part`]s, pick a [`WireFormat`] in the
//! [`Config`], and get streamed [`Event`]s plus a final [`Response`] whose assistant message can
//! be appended to the history as it is (reasoning signatures and encrypted reasoning included).

mod client;
mod sse;
mod types;
pub mod wire;

pub use client::{Client, Config};
pub use types::*;
