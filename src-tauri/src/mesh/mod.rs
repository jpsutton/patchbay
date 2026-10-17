//! Peer-to-peer mesh: membership, data sync, remote control and session handover. The protocol
//! pieces shared with the phone app live in the `patchbay-mesh` crate.
//!
//! Design: docs/design/mesh.md and docs/design/mobile.md.

pub mod keys;
pub mod members;
pub mod node;
pub mod secrets;
pub mod sync;

pub use node::{Mesh, RpcHandler};
pub use patchbay_mesh::{handshake, remote, wire};

#[cfg(test)]
mod node_tests;
