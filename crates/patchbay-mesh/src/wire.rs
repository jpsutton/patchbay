//! Messages exchanged inside an established (encrypted) mesh connection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::member::Member;

pub const PROTOCOL_VERSION: u32 = 1;

/// In `Hello::features`: this node serves remote members (see docs/design/mobile.md).
pub const FEATURE_GATEWAY: &str = "remote-gateway";

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
    /// `Some("remote")` for a remote-only member. Older nodes don't send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Optional abilities, such as [`FEATURE_GATEWAY`]. Older nodes don't send any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
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
    /// (peer node, URL it was reached at), so others can try the same address directly.
    #[serde(default)]
    pub peers: Vec<(String, String)>,
    /// Phones using this node as their gateway right now. Older nodes omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<String>,
}

/// One synced cell write (or a row delete when `c` is the tombstone column).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Op {
    /// Table.
    pub t: String,
    /// Primary key: the value itself, or a JSON array for composite keys.
    pub k: String,
    /// Column.
    pub c: String,
    /// Value (null for tombstones).
    #[serde(default)]
    pub v: Value,
    pub h: i64,
    /// Origin node.
    pub o: String,
    pub s: i64,
}

pub type VersionVector = BTreeMap<String, i64>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    Hello(Box<Hello>),
    /// To a joining node: waiting for a member to approve; shows the confirmation code.
    JoinPending { code: String },
    /// `data_key` is empty for a remote member.
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
    /// From a gateway to a remote member: reconnect with this instead of the passphrase
    /// handshake. `id` and `key` are hex; `expires` is in Unix milliseconds.
    Ticket { id: String, key: String, expires: i64 },
}
