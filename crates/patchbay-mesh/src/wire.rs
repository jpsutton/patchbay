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

/// Live event: a node asks to join and waits on `host`, a member. Every member that gets it may
/// decide the request, not only the host. Payload: [`JoinRequest`]. Sent as an event, not a
/// message of its own, so nodes that predate it ignore it.
pub const EV_JOIN_REQUEST: &str = "mesh://join-request";
/// Live event: the request is decided or gone (`nodeId`). Sent by the host.
pub const EV_JOIN_RESOLVED: &str = "mesh://join-resolved";

/// A node waiting to be admitted, as its host announces it to the other members and phones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JoinRequest {
    pub node_id: String,
    pub name: String,
    /// The six-digit code both the joiner and the host show.
    pub code: String,
    /// What the approver signs into the admission record.
    pub sign_pub: String,
    pub noise_pub: String,
    /// `Some("remote")` for a phone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The member the node is connected to; the admission goes back to it.
    pub host: String,
}

impl JoinRequest {
    /// Whether `code` is the one the host's key and the joiner's key give. An approver checks
    /// this before it signs, so the code it shows is tied to the key it admits.
    pub fn code_matches(&self, host_noise_pub: &str) -> bool {
        use crate::keys::hex32;
        match (hex32(host_noise_pub), hex32(&self.noise_pub)) {
            (Ok(h), Ok(j)) => crate::handshake::confirmation_code(&h, &j) == self.code,
            _ => false,
        }
    }
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
    /// App version of the origin. Older nodes omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
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
