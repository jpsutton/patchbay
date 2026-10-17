//! Patchbay's mesh protocol, shared by the desktop app and the phone app: the connection
//! handshakes, wire messages, member records, the live-view resume logic, and the client a remote
//! member uses to reach the mesh through a gateway.
//!
//! Design: docs/design/mesh.md and docs/design/mobile.md.

pub mod client;
pub mod handshake;
pub mod keys;
pub mod member;
pub mod remote;
pub mod wire;
pub mod ws;
