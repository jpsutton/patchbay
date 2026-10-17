//! Messages exchanged inside an established (encrypted) mesh connection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::members::Member;
use super::sync::{Op, VersionVector};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub node_id: String,
    pub name: String,
    pub sign_pub: String,
    pub version: u32,
    /// The mesh this node belongs to, if any.
    pub mesh_id: Option<String>,
    /// This node's own admission record, so members who haven't synced it yet can check it.
    pub record: Option<Member>,
    /// Asking to be admitted.
    pub joining: bool,
    pub data_key_gen: u32,
    /// URLs this node can be reached at.
    pub addrs: Vec<String>,
    /// Port of its listener, to derive a URL from the address it connected from.
    pub listen_port: Option<u16>,
}

/// One node's direct connections, flooded through the mesh so every node can find relay paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkReport {
    pub origin: String,
    pub ver: i64,
    /// (peer node, round-trip ms)
    pub links: Vec<(String, u32)>,
    pub addrs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    Hello(Box<Hello>),
    /// To a joining node: waiting for a member to approve; shows the confirmation code.
    JoinPending { code: String },
    Admitted { members: Vec<Member>, data_key: String },
    Rejected { reason: String },
    DataKey { key: String },
    SyncVv { vv: VersionVector },
    SyncOps { ops: Vec<Op>, upto: VersionVector },
    /// A live event for the UI (agent output, chat stream deltas).
    Event { name: String, payload: Value },
    Rpc { id: u64, method: String, params: Value },
    RpcResult { id: u64, ok: Option<Value>, err: Option<String> },
    Links(LinkReport),
    /// A frame of a connection tunnelled through this node (base64), or its end.
    Relay { circuit: u64, from: String, to: String, ttl: u8, data: Option<String>, close: bool },
    Ping { t: i64 },
    Pong { t: i64 },
}
