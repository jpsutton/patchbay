//! Peer-to-peer mesh: membership, data sync, remote control and session handover.
//!
//! Design: docs/design/mesh.md.

pub mod handshake;
pub mod keys;
pub mod members;
pub mod node;
pub mod secrets;
pub mod sync;
pub mod wire;

pub use node::{Mesh, RpcHandler};

#[cfg(test)]
mod node_tests;
