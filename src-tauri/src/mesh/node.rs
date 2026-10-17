//! The running mesh node: listener, dialer, sessions with peers, sync, relaying and RPC.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use patchbay_mesh::ws::{normalize_url, recv, send, ws_link};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::handshake::{self, HandshakeError, Link, Secure, TICKET_ID_LEN};
use super::keys::{Identity, KeyStore, random32};
use super::members::{self, Member, ROLE_REMOTE};
use super::remote::{self as rmt, Live, Numbering, Ring};
use super::secrets::{self, DataKey};
use super::sync::{self, Applied, LocalDefaults, Op, VersionVector};
use super::wire::{FEATURE_GATEWAY, Hello, LinkReport, Msg, PROTOCOL_VERSION};
use crate::db::{Db, now_ms};

pub use patchbay_mesh::ws::DEFAULT_PORT;
const PASSPHRASE: &str = "mesh:passphrase";
/// How long a remote member's resume ticket stays valid.
const TICKET_MS: i64 = 24 * 3600 * 1000;
/// A remote member pings every 15 s in the foreground and every 60 s in the background.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(200);
/// Event rings for topics nothing touched for this long are dropped.
const RING_IDLE: Duration = Duration::from_secs(2 * 3600);
const DATA_KEY: &str = "mesh:data_key";
const SYNC_BATCH: usize = 2000;
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a chat stream may go quiet after its streamer disconnects before it is saved as
/// interrupted.
const STREAM_GRACE_MS: i64 = if cfg!(test) { 1_000 } else { 15_000 };
const HOUSEKEEPING: Duration = Duration::from_millis(if cfg!(test) { 500 } else { 5_000 });
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Delivers an event to the UI: (event name, payload).
pub type UiSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// App-level requests from other nodes (remote control), and hooks into the app.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait RpcHandler: Send + Sync {
    async fn call(&self, from: String, method: String, params: Value) -> Result<Value>;
    /// Remote changes were written to the database.
    fn synced(&self, _applied: &Applied) {}
    /// A live event arrived from another node (before it goes to the UI).
    fn event(&self, _name: &str, _payload: &Value) {}
}

#[derive(Clone)]
pub struct Mesh(Arc<Inner>);

struct Inner {
    db: Arc<Db>,
    data_dir: PathBuf,
    me: String,
    ident: Identity,
    keys: Arc<dyn KeyStore>,
    ui: RwLock<Option<UiSink>>,
    handler: RwLock<Option<Arc<dyn RpcHandler>>>,
    st: Mutex<State>,
    /// Bumped whenever the database has changes peers may lack.
    changed: watch::Sender<u64>,
    rpc_waiters: Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    run: Mutex<Option<CancellationToken>>,
    /// Port actually bound (tests bind port 0).
    bound_port: Mutex<Option<u16>>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Sess>,
    pending: HashMap<String, Pending>,
    joining: Option<Joining>,
    links: HashMap<String, LinkReport>,
    /// Failed handshakes per source: (count, blocked until).
    blocked: HashMap<String, (u32, Instant)>,
    /// Relayed connections that end at this node: circuit -> inbound frames.
    circuits: HashMap<u64, mpsc::UnboundedSender<Vec<u8>>>,
    /// URL -> (failures, next attempt).
    backoff: HashMap<String, (u32, Instant)>,
    dialing: HashSet<String>,
    relaying_to: HashSet<String>,
    /// Nodes never dialed directly (tests use it to force relaying).
    no_direct: HashSet<String>,
    /// Known URLs per node.
    addrs: HashMap<String, BTreeSet<String>>,
    streams: HashMap<String, Stream>,
    /// Remote members (phones) connected to this node as their gateway.
    remotes: HashMap<String, RemoteSess>,
    tickets: HashMap<[u8; TICKET_ID_LEN], Ticket>,
    /// Recent live events per topic, for remote members resuming; and when each was last touched.
    rings: HashMap<String, (Ring, Instant)>,
    /// Points for the live events this node originates.
    numbering: Numbering,
    /// What the session list shows per Code session, from the events seen here.
    live: HashMap<String, Live>,
}

struct RemoteSess {
    id: u64,
    tx: mpsc::UnboundedSender<Msg>,
    kill: CancellationToken,
    /// Topics it follows.
    subs: HashSet<String>,
}

struct Ticket {
    node: String,
    noise_pub: [u8; 32],
    key: [u8; 32],
    expires: i64,
}

struct Sess {
    id: u64,
    tx: mpsc::UnboundedSender<Msg>,
    kill: CancellationToken,
    via: Option<String>,
    url: Option<String>,
    rtt: Option<u32>,
    /// Node that opened the connection (decides which of two simultaneous connections survives).
    opener: String,
}

struct Pending {
    name: String,
    code: String,
    sign_pub: String,
    noise_pub: String,
    role: Option<String>,
    tx: mpsc::UnboundedSender<Msg>,
}

struct Joining {
    url: String,
    passphrase: String,
    code: Option<String>,
    status: String,
    error: Option<String>,
}

/// A chat reply streaming on some node, as seen here.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub conversation_id: String,
    pub streamer: String,
    pub streamer_name: String,
    pub user_message_id: Option<String>,
    pub assistant_message_id: Option<String>,
    pub text: String,
    pub reasoning: String,
    pub status: Option<String>,
    #[serde(skip)]
    done: bool,
    #[serde(skip)]
    lost_at: Option<i64>,
}

struct Defaults {
    data_dir: PathBuf,
}

impl LocalDefaults for Defaults {
    fn local_defaults(&self, table: &str, cells: &HashMap<String, Value>) -> Vec<(&'static str, Value)> {
        if table != "attachments" {
            return Vec::new();
        }
        let sha = cells.get("sha256").and_then(Value::as_str).unwrap_or_default();
        let name = cells.get("name").and_then(Value::as_str).unwrap_or_default();
        let path = crate::attachments::stored_path(&self.data_dir, sha, name);
        vec![("path", Value::String(path.to_string_lossy().into_owned()))]
    }
}

impl Mesh {
    pub fn new(db: Arc<Db>, data_dir: PathBuf, keys: Arc<dyn KeyStore>) -> Result<Self> {
        let ident = Identity::load_or_create(&data_dir)?;
        let me = db.node_id();
        let (changed, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            db,
            data_dir,
            me,
            ident,
            keys,
            ui: RwLock::new(None),
            handler: RwLock::new(None),
            st: Mutex::new(State::default()),
            changed,
            rpc_waiters: Mutex::default(),
            next_id: AtomicU64::new(1),
            run: Mutex::new(None),
            bound_port: Mutex::new(None),
        });
        inner.load_addrs();
        Ok(Mesh(inner))
    }

    pub fn set_ui(&self, ui: UiSink) {
        *self.0.ui.write().unwrap() = Some(ui);
    }

    pub fn set_handler(&self, h: Arc<dyn RpcHandler>) {
        *self.0.handler.write().unwrap() = Some(h);
    }

    pub fn node_id(&self) -> &str {
        &self.0.me
    }

    #[cfg(test)]
    pub fn name(&self) -> String {
        self.0.name()
    }

    pub fn is_member(&self) -> bool {
        self.0.my_record().is_some_and(|m| !m.is_removed())
    }

    #[cfg(test)]
    pub fn bound_port(&self) -> Option<u16> {
        *self.0.bound_port.lock().unwrap()
    }

    pub fn running(&self) -> bool {
        self.0.run.lock().unwrap().is_some()
    }

    /// Start networking if this node is a member (or joining).
    pub fn start(&self) {
        let inner = self.0.clone();
        let mut run = inner.run.lock().unwrap();
        if run.is_some() {
            return;
        }
        let token = CancellationToken::new();
        *run = Some(token.clone());
        drop(run);
        tokio::spawn(inner.clone().listen(token.clone()));
        tokio::spawn(inner.clone().connector(token.clone()));
        tokio::spawn(inner.clone().watch_db(token.clone()));
        tokio::spawn(inner.clone().housekeeping(token));
        self.0.emit_status();
    }

    pub fn stop(&self) {
        if let Some(t) = self.0.run.lock().unwrap().take() {
            t.cancel();
        }
        let mut st = self.0.st.lock().unwrap();
        for s in st.sessions.values() {
            s.kill.cancel();
        }
        for r in st.remotes.values() {
            r.kill.cancel();
        }
        st.sessions.clear();
        st.remotes.clear();
        st.pending.clear();
        st.circuits.clear();
        drop(st);
        *self.0.bound_port.lock().unwrap() = None;
        self.0.emit_status();
    }

    /// Found a new mesh with this node as its first member.
    pub fn create(&self, passphrase: &str, name: &str) -> Result<()> {
        let inner = &self.0;
        check_passphrase(passphrase)?;
        if self.is_member() {
            bail!("this node is already in a mesh; leave it first");
        }
        let mesh_id = hex::encode(&random32()[..16]);
        inner.set_name(name)?;
        let rec = Member::admit(
            &inner.me,
            &inner.ident,
            &mesh_id,
            &inner.me,
            &inner.name(),
            &inner.ident.sign_pub_hex(),
            &inner.ident.noise_pub_hex(),
            None,
        );
        members::upsert(&inner.db, &rec)?;
        if !inner.keys.set(PASSPHRASE, passphrase) {
            bail!("could not store the mesh passphrase");
        }
        let dk = DataKey { generation: 1, key: random32() };
        inner.keys.set(DATA_KEY, &dk.encode());
        secrets::reseal(&inner.db, inner.keys.as_ref(), &dk)?;
        self.start();
        Ok(())
    }

    /// Ask to join the mesh reachable at `url`. A member must approve before data flows.
    pub fn join(&self, url: &str, passphrase: &str, name: &str) -> Result<()> {
        check_passphrase(passphrase)?;
        if self.is_member() {
            bail!("this node is already in a mesh; leave it first");
        }
        self.0.set_name(name)?;
        self.0.st.lock().unwrap().joining = Some(Joining {
            url: normalize_url(url),
            passphrase: passphrase.to_owned(),
            code: None,
            status: "connecting".into(),
            error: None,
        });
        if self.running() {
            self.0.emit_status();
        } else {
            self.start();
        }
        Ok(())
    }

    pub fn cancel_join(&self) {
        let was = self.0.st.lock().unwrap().joining.take();
        if was.is_some() && !self.is_member() {
            self.stop();
        }
        self.0.emit_status();
    }

    pub fn approve(&self, node_id: &str) -> Result<()> {
        self.0.approve(node_id)
    }

    pub fn reject(&self, node_id: &str) {
        if let Some(p) = self.0.st.lock().unwrap().pending.remove(node_id) {
            let _ = p.tx.send(Msg::Rejected { reason: "a member declined the request".into() });
        }
        self.0.emit_status();
    }

    /// Remove a member: sign its removal, drop its connection and rotate the data key.
    pub fn remove(&self, node_id: &str) -> Result<()> {
        let inner = &self.0;
        let mut m = members::get(&inner.db, node_id)?.ok_or_else(|| anyhow!("not a member"))?;
        if m.is_removed() {
            return Ok(());
        }
        m.mark_removed(&inner.me, &inner.ident);
        members::upsert(&inner.db, &m)?;
        if node_id == inner.me {
            return Ok(());
        }
        inner.drop_session_soon(node_id);
        // A phone never held the data key, so nothing it had needs replacing.
        if m.is_remote() {
            inner.emit_status();
            return Ok(());
        }
        let old = inner.data_key();
        let dk = DataKey { generation: old.map(|d| d.generation).unwrap_or(0) + 1, key: random32() };
        inner.adopt_data_key(dk.clone())?;
        inner.broadcast(Msg::DataKey { key: dk.encode() });
        inner.emit_status();
        Ok(())
    }

    /// Leave the mesh: sign this node's removal (sent if any peer is connected), then forget the
    /// passphrase and data key. Local data stays.
    pub async fn leave(&self) -> Result<()> {
        if self.is_member() {
            self.remove(&self.0.me.clone())?;
            // Give connected peers a moment to receive the removal.
            self.0.changed.send_modify(|v| *v += 1);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.stop();
        self.0.keys.delete(PASSPHRASE);
        self.0.keys.delete(DATA_KEY);
        self.0.st.lock().unwrap().joining = None;
        let db = &self.0.db;
        db.set_setting("mesh.addrs", "{}")?;
        self.0.emit_status();
        Ok(())
    }

    pub fn set_config(&self, name: Option<&str>, listen: Option<&str>, advertise: Option<Vec<String>>) -> Result<()> {
        if let Some(n) = name {
            self.0.set_name(n)?;
            // Members show each other's names from the synced record.
            if let Some(mut rec) = self.0.my_record() {
                rec.name = self.0.name();
                members::upsert(&self.0.db, &rec)?;
            }
        }
        let mut restart = false;
        if let Some(l) = listen {
            let l = l.trim();
            if !l.is_empty() {
                l.parse::<std::net::SocketAddr>().context("listen address must look like 0.0.0.0:41890")?;
            }
            restart = self.0.db.get_setting("mesh.listen")?.as_deref().unwrap_or("") != l;
            self.0.db.set_setting("mesh.listen", l)?;
        }
        if let Some(a) = advertise {
            let urls: Vec<String> = a.iter().filter(|u| !u.trim().is_empty()).map(|u| normalize_url(u)).collect();
            self.0.db.set_setting("mesh.advertise", &serde_json::to_string(&urls)?)?;
        }
        if restart && self.running() {
            self.stop();
            self.start();
        }
        self.0.emit_status();
        Ok(())
    }

    /// Remember another address to reach the mesh at.
    pub fn add_address(&self, url: &str) -> Result<()> {
        let mut seeds = self.0.seeds();
        seeds.insert(normalize_url(url));
        self.0.db.set_setting("mesh.seeds", &serde_json::to_string(&seeds)?)?;
        Ok(())
    }

    pub fn status(&self) -> Value {
        self.0.status()
    }

    /// Send a live event to one member; false when it isn't connected.
    pub fn send_event(&self, node: &str, name: &str, payload: Value) -> bool {
        let st = self.0.st.lock().unwrap();
        st.sessions
            .get(node)
            .is_some_and(|s| s.tx.send(Msg::Event { name: name.to_owned(), payload }).is_ok())
    }

    /// Send a live event this node originates to every connected member, and to the phones
    /// following its topic. Returns the payload as sent: with its point, when it has a topic.
    pub fn broadcast_event(&self, name: &str, mut payload: Value) -> Value {
        self.0.st.lock().unwrap().numbering.stamp(name, &mut payload);
        self.0.fanout(name, &payload);
        self.0.broadcast(Msg::Event { name: name.to_owned(), payload: payload.clone() });
        payload
    }

    /// What the session list shows for each Code session, as seen from here.
    pub fn live(&self) -> HashMap<String, Live> {
        self.0.st.lock().unwrap().live.clone()
    }

    /// Every chat reply streaming right now, here or elsewhere.
    pub fn all_streams(&self) -> Vec<Stream> {
        self.0.st.lock().unwrap().streams.values().filter(|s| !s.done).cloned().collect()
    }

    /// Member records, known addresses of full members, and who this node reaches: what a phone
    /// needs to find another gateway later.
    pub fn remote_members(&self) -> Value {
        let inner = &self.0;
        let mesh = inner.mesh_id();
        let mems: Vec<Member> =
            members::list(&inner.db).unwrap_or_default().into_iter().filter(|m| Some(&m.mesh_id) == mesh.as_ref()).collect();
        let st = inner.st.lock().unwrap();
        let mut addrs: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut online: HashMap<String, &str> = HashMap::new();
        for m in mems.iter().filter(|m| !m.is_remote() && !m.is_removed()) {
            if m.node_id == inner.me {
                let mine: BTreeSet<String> = inner.advertise().into_iter().collect();
                addrs.insert(m.node_id.clone(), mine);
                online.insert(m.node_id.clone(), "direct");
            } else {
                addrs.insert(m.node_id.clone(), st.addrs.get(&m.node_id).cloned().unwrap_or_default());
                online.insert(m.node_id.clone(), if st.sessions.contains_key(&m.node_id) { "relay" } else { "offline" });
            }
        }
        json!({ "members": mems, "addrs": addrs, "online": online })
    }

    /// Call `method` on another node.
    pub async fn rpc(&self, node: &str, method: &str, params: Value) -> Result<Value> {
        self.0.rpc(node, method, params, RPC_TIMEOUT).await
    }

    /// An RPC that may take a while (moving a session).
    pub async fn rpc_long(&self, node: &str, method: &str, params: Value) -> Result<Value> {
        self.0.rpc(node, method, params, Duration::from_secs(30 * 60)).await
    }

    /// Never dial `node` directly (tests).
    #[cfg(test)]
    pub fn block_direct(&self, node: &str) {
        self.0.st.lock().unwrap().no_direct.insert(node.to_owned());
    }

    /// Cut a phone's connection, as a network change would (tests).
    #[cfg(test)]
    pub fn drop_remote(&self, node: &str) {
        if let Some(r) = self.0.st.lock().unwrap().remotes.remove(node) {
            r.kill.cancel();
        }
    }

    pub fn connected(&self, node: &str) -> bool {
        self.0.st.lock().unwrap().sessions.contains_key(node)
    }

    pub fn member_name(&self, node: &str) -> String {
        self.0.member_name(node)
    }

    /// A local chat stream event: track it and send it to every member.
    pub fn chat_event(&self, conversation_id: &str, event: Value) {
        let mut payload = json!({
            "conversationId": conversation_id,
            "streamer": self.0.me,
            "streamerName": self.0.name(),
            "event": event,
        });
        self.0.st.lock().unwrap().numbering.stamp(rmt::EV_CHAT, &mut payload);
        self.0.track_stream(&payload);
        self.0.fanout(rmt::EV_CHAT, &payload);
        self.0.broadcast(Msg::Event { name: rmt::EV_CHAT.into(), payload });
    }

    /// Chat replies streaming on other nodes right now.
    pub fn remote_streams(&self) -> Vec<Stream> {
        let me = &self.0.me;
        self.0.st.lock().unwrap().streams.values().filter(|s| &s.streamer != me && !s.done).cloned().collect()
    }

    /// Node streaming a reply in this conversation, if any.
    pub fn streamer_of(&self, conversation_id: &str) -> Option<String> {
        self.0.st.lock().unwrap().streams.get(conversation_id).filter(|s| !s.done).map(|s| s.streamer.clone())
    }

    /// Re-seal local secrets after the user changed a key.
    pub fn secrets_changed(&self) {
        if let Some(dk) = self.0.data_key()
            && let Err(e) = secrets::reseal(&self.0.db, self.0.keys.as_ref(), &dk)
        {
            tracing::warn!("mesh: sealing secrets: {e:#}");
        }
    }

    /// Make sure an attachment's bytes are on this node, fetching them from a peer if needed.
    pub async fn ensure_file(&self, sha: &str, name: &str) -> Result<PathBuf> {
        self.0.clone().ensure_file(sha, name).await
    }

}

fn check_passphrase(p: &str) -> Result<()> {
    if p.chars().count() < 8 {
        bail!("use a passphrase of at least 8 characters");
    }
    Ok(())
}

impl Inner {
    fn ui(&self, name: &str, payload: Value) {
        if let Some(ui) = self.ui.read().unwrap().as_ref() {
            ui(name, payload);
        }
    }

    fn emit_status(&self) {
        self.ui("mesh://status", Value::Null);
    }

    fn name(&self) -> String {
        self.db
            .get_setting("mesh.name")
            .ok()
            .flatten()
            .filter(|n| !n.trim().is_empty())
            .or_else(hostname)
            .unwrap_or_else(|| format!("node-{}", &self.me[..6]))
    }

    fn set_name(&self, name: &str) -> Result<()> {
        let n = name.trim();
        if n.chars().count() > 64 {
            bail!("name is too long");
        }
        self.db.set_setting("mesh.name", n)?;
        Ok(())
    }

    fn my_record(&self) -> Option<Member> {
        members::get(&self.db, &self.me).ok().flatten()
    }

    fn mesh_id(&self) -> Option<String> {
        self.my_record().filter(|m| !m.is_removed()).map(|m| m.mesh_id)
    }

    fn member_name(&self, node: &str) -> String {
        members::get(&self.db, node).ok().flatten().map(|m| m.name).unwrap_or_else(|| node.chars().take(8).collect())
    }

    fn data_key(&self) -> Option<DataKey> {
        self.keys.get(DATA_KEY).and_then(|s| DataKey::decode(&s))
    }

    fn adopt_data_key(&self, dk: DataKey) -> Result<()> {
        if self.data_key().is_some_and(|cur| cur.rank() >= dk.rank()) {
            return Ok(());
        }
        if !self.keys.set(DATA_KEY, &dk.encode()) {
            bail!("could not store the mesh data key");
        }
        secrets::install_all(&self.db, self.keys.as_ref(), &dk)?;
        secrets::reseal(&self.db, self.keys.as_ref(), &dk)?;
        Ok(())
    }

    fn listen_addr(&self) -> String {
        self.db
            .get_setting("mesh.listen")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"))
    }

    fn advertise(&self) -> Vec<String> {
        self.db
            .get_setting("mesh.advertise")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn seeds(&self) -> BTreeSet<String> {
        self.db
            .get_setting("mesh.seeds")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn load_addrs(&self) {
        let saved: HashMap<String, BTreeSet<String>> = self
            .db
            .get_setting("mesh.addrs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        self.st.lock().unwrap().addrs = saved;
    }

    fn learn_addrs(&self, node: &str, urls: impl IntoIterator<Item = String>) {
        let mut st = self.st.lock().unwrap();
        let set = st.addrs.entry(node.to_owned()).or_default();
        let before = set.len();
        for u in urls {
            set.insert(normalize_url(&u));
        }
        if set.len() != before {
            let json = serde_json::to_string(&st.addrs).unwrap_or_default();
            drop(st);
            let _ = self.db.set_setting("mesh.addrs", &json);
        }
    }

    fn status(&self) -> Value {
        let mems = members::list(&self.db).unwrap_or_default();
        let my = mems.iter().find(|m| m.node_id == self.me).cloned();
        let mesh_id = my.as_ref().filter(|m| !m.is_removed()).map(|m| m.mesh_id.clone());
        let st = self.st.lock().unwrap();
        let members: Vec<Value> = mems
            .iter()
            .filter(|m| Some(&m.mesh_id) == mesh_id.as_ref())
            .map(|m| {
                let sess = st.sessions.get(&m.node_id);
                let state = if m.node_id == self.me {
                    "self"
                } else if m.is_removed() {
                    "removed"
                } else if m.is_remote() {
                    if st.remotes.contains_key(&m.node_id) { "direct" } else { "offline" }
                } else {
                    match sess {
                        Some(s) if s.via.is_some() => "relay",
                        Some(_) => "direct",
                        None => "offline",
                    }
                };
                let via = sess.and_then(|s| s.via.clone());
                json!({
                    "nodeId": m.node_id,
                    "name": m.name,
                    "state": state,
                    "via": via,
                    "viaName": via.as_ref().map(|v| mems.iter().find(|x| &x.node_id == v).map(|x| x.name.clone()).unwrap_or_default()),
                    "rttMs": sess.and_then(|s| s.rtt),
                    "url": sess.and_then(|s| s.url.clone()),
                    "admittedAt": m.admitted_at,
                    "approver": m.approver,
                    "role": m.role,
                })
            })
            .collect();
        let pending: Vec<Value> = st
            .pending
            .iter()
            .map(|(id, p)| json!({ "nodeId": id, "name": p.name, "code": p.code, "role": p.role }))
            .collect();
        let joining = st.joining.as_ref().map(|j| {
            json!({ "url": j.url, "code": j.code, "status": j.status, "error": j.error })
        });
        drop(st);
        json!({
            "nodeId": self.me,
            "name": self.name(),
            "member": mesh_id.is_some(),
            "meshId": mesh_id,
            "running": self.run.lock().unwrap().is_some(),
            "listen": self.listen_addr(),
            "port": *self.bound_port.lock().unwrap(),
            "advertise": self.advertise(),
            "joining": joining,
            "members": members,
            "pending": pending,
        })
    }

    // ---------------- tasks ----------------

    async fn listen(self: Arc<Self>, token: CancellationToken) {
        let addr = self.listen_addr();
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("mesh: cannot listen on {addr}: {e}");
                self.ui("mesh://error", json!({ "message": format!("Can't listen on {addr}: {e}") }));
                return;
            }
        };
        *self.bound_port.lock().unwrap() = listener.local_addr().ok().map(|a| a.port());
        self.emit_status();
        loop {
            let (tcp, peer) = tokio::select! {
                _ = token.cancelled() => return,
                r = listener.accept() => match r {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!("mesh: accept: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                }
            };
            let inner = self.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let ip = peer.ip();
                if inner.is_blocked(&ip.to_string()) {
                    return;
                }
                let ws = match tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::accept_async(tcp)).await {
                    Ok(Ok(ws)) => ws,
                    _ => return,
                };
                let link = ws_link(ws);
                if let Err(e) = inner.clone().run_conn(link, false, Some(ip), None, None, token).await {
                    tracing::debug!("mesh: inbound from {ip}: {e:#}");
                }
            });
        }
    }

    async fn connector(self: Arc<Self>, token: CancellationToken) {
        loop {
            self.clone().dial_round(&token);
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
        }
    }

    fn dial_round(self: Arc<Self>, token: &CancellationToken) {
        let mut urls: Vec<(String, Option<String>)> = Vec::new();
        let joining_url = self
            .st
            .lock()
            .unwrap()
            .joining
            .as_ref()
            .filter(|j| j.status == "connecting")
            .map(|j| j.url.clone());
        if let Some(u) = joining_url {
            urls.push((u, None));
        }
        let member = self.mesh_id().is_some();
        let mut relay_targets = Vec::new();
        if member {
            let mems = members::list(&self.db).unwrap_or_default();
            let st = self.st.lock().unwrap();
            // Phones dial us; they never listen.
            for m in mems.iter().filter(|m| m.node_id != self.me && !m.is_removed() && !m.is_remote()) {
                let sess = st.sessions.get(&m.node_id);
                if sess.is_some_and(|s| s.via.is_none()) {
                    continue;
                }
                let direct = if st.no_direct.contains(&m.node_id) { None } else { st.addrs.get(&m.node_id) };
                for u in direct.into_iter().flatten() {
                    urls.push((u.clone(), Some(m.node_id.clone())));
                }
                if sess.is_none() {
                    relay_targets.push(m.node_id.clone());
                }
            }
            drop(st);
            // Seeds help a member find the mesh again when no known address works.
            if self.st.lock().unwrap().sessions.is_empty() {
                for s in self.seeds() {
                    urls.push((s, None));
                }
            }
        }
        let now = Instant::now();
        for (url, node) in urls {
            {
                let mut st = self.st.lock().unwrap();
                if st.dialing.contains(&url) || st.backoff.get(&url).is_some_and(|(_, until)| *until > now) {
                    continue;
                }
                st.dialing.insert(url.clone());
            }
            let inner = self.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let res = inner.clone().dial(&url, token).await;
                let _ = node;
                let mut st = inner.st.lock().unwrap();
                st.dialing.remove(&url);
                match res {
                    Ok(()) => {
                        st.backoff.remove(&url);
                    }
                    Err(e) => {
                        tracing::debug!("mesh: dial {url}: {e:#}");
                        let n = st.backoff.get(&url).map(|b| b.0).unwrap_or(0) + 1;
                        let wait = Duration::from_secs((2u64.pow(n.min(8))).min(120));
                        st.backoff.insert(url.clone(), (n, Instant::now() + wait));
                        if let Some(j) = st.joining.as_mut()
                            && j.url == url
                            && j.status == "connecting"
                        {
                            j.error = Some(format!("{e:#}"));
                        }
                        drop(st);
                        inner.emit_status();
                    }
                }
            });
        }
        // No direct path: go through a member that can reach the target.
        for target in relay_targets {
            let Some(hop) = self.next_hop(&target) else { continue };
            {
                let mut st = self.st.lock().unwrap();
                if st.sessions.contains_key(&target) || !st.relaying_to.insert(target.clone()) {
                    continue;
                }
            }
            let inner = self.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let circuit = rand::random::<u64>();
                let link = inner.clone().circuit_link(circuit, target.clone());
                let r = inner.clone().run_conn(link, true, None, None, Some(hop), token).await;
                inner.st.lock().unwrap().relaying_to.remove(&target);
                if let Err(e) = r {
                    tracing::debug!("mesh: relay to {target}: {e:#}");
                }
            });
        }
    }

    /// Connect to `url` and run the session there; the URL counts as busy until it ends.
    async fn dial(self: Arc<Self>, url: &str, token: CancellationToken) -> Result<()> {
        let (ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(url))
            .await
            .map_err(|_| anyhow!("timed out"))??;
        self.run_conn(ws_link(ws), true, None, Some(url.to_owned()), None, token).await
    }

    /// Poll for local writes (the triggers bump `sync_meta.seq`) and tell sessions.
    async fn watch_db(self: Arc<Self>, token: CancellationToken) {
        let mut last = -1i64;
        loop {
            let db = self.db.clone();
            let seq = tokio::task::spawn_blocking(move || {
                db.with_conn(|c| c.query_row("SELECT seq FROM sync_meta", [], |r| r.get::<_, i64>(0)).unwrap_or(0))
            })
            .await
            .unwrap_or(0);
            if seq != last {
                last = seq;
                self.changed.send_modify(|v| *v += 1);
            }
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(150)) => {}
            }
        }
    }

    async fn housekeeping(self: Arc<Self>, token: CancellationToken) {
        let mut tick = 0u64;
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(HOUSEKEEPING) => {}
            }
            tick += 1;
            if tick.is_multiple_of(2) {
                self.send_links();
            }
            self.finish_lost_streams();
            {
                let mut st = self.st.lock().unwrap();
                let now = now_ms();
                st.tickets.retain(|_, t| t.expires > now);
                st.rings.retain(|_, (_, touched)| touched.elapsed() < RING_IDLE);
            }
            if tick % 720 == 1 {
                let db = self.db.clone();
                let _ = tokio::task::spawn_blocking(move || db.with_conn(|c| sync::prune_stale_clocks(c))).await;
            }
        }
    }

    fn is_blocked(&self, key: &str) -> bool {
        self.st.lock().unwrap().blocked.get(key).is_some_and(|(_, until)| *until > Instant::now())
    }

    /// Slow down passphrase guessing: each failure doubles the wait for that source.
    fn record_failure(&self, key: &str) {
        let mut st = self.st.lock().unwrap();
        let n = st.blocked.get(key).map(|b| b.0).unwrap_or(0) + 1;
        let wait = Duration::from_secs(2u64.pow(n.min(9)).min(300));
        st.blocked.insert(key.to_owned(), (n, Instant::now() + wait));
    }

    // ---------------- connections ----------------

    /// Handshake, identify, then run a session until it ends.
    async fn run_conn(
        self: Arc<Self>,
        mut link: Link,
        initiator: bool,
        ip: Option<IpAddr>,
        url: Option<String>,
        via: Option<String>,
        token: CancellationToken,
    ) -> Result<()> {
        let source = ip.map(|i| i.to_string()).or_else(|| via.clone().map(|v| format!("via:{v}"))).unwrap_or_default();
        let joining_pass = self.st.lock().unwrap().joining.as_ref().map(|j| j.passphrase.clone());
        let pass = match (self.keys.get(PASSPHRASE).filter(|_| self.mesh_id().is_some()), joining_pass) {
            (Some(p), _) => p,
            (None, Some(p)) => p,
            _ => bail!("no mesh passphrase"),
        };
        let hs = if initiator {
            handshake::initiate(&mut link, &pass, &self.ident.noise_private).await.map(|s| (s, None))
        } else {
            let inner = self.clone();
            handshake::respond_any(&mut link, &pass, &self.ident.noise_private, move |id| inner.ticket_key(id)).await
        };
        let (mut sec, ticket) = match hs {
            Ok(s) => s,
            Err(HandshakeError::WrongPassphrase) => {
                if !initiator {
                    self.record_failure(&source);
                }
                self.set_join_error(url.as_deref(), "The passphrase doesn't match this mesh's");
                bail!("passphrase mismatch");
            }
            // A phone with a stale ticket; it retries with the passphrase.
            Err(HandshakeError::UnknownTicket) => bail!("unknown resume ticket"),
            Err(HandshakeError::Other(e)) => return Err(e),
        };
        // A ticket stands in for the passphrase only for the device it was issued to.
        let ticket_node = match ticket.and_then(|id| self.st.lock().unwrap().tickets.get(&id).map(|t| (t.node.clone(), t.noise_pub))) {
            Some((node, key)) if key == sec.remote_static => Some(node),
            Some(_) => bail!("resume ticket presented with other keys"),
            None if ticket.is_some() => bail!("resume ticket expired"),
            None => None,
        };
        let hello = self.hello();
        send(&mut sec, &link, &Msg::Hello(Box::new(hello)))?;
        let theirs = match tokio::time::timeout(Duration::from_secs(15), recv(&mut sec, &mut link)).await {
            Ok(Ok(Some(Msg::Hello(h)))) => *h,
            _ => bail!("no hello"),
        };
        if theirs.node_id == self.me {
            bail!("peer has this node's own id (a copied database?)");
        }
        if theirs.version != PROTOCOL_VERSION {
            bail!("peer speaks mesh protocol {}", theirs.version);
        }
        if ticket_node.as_ref().is_some_and(|n| n != &theirs.node_id) {
            bail!("resume ticket belongs to another device");
        }
        if theirs.role.as_deref() == Some(ROLE_REMOTE) {
            return self.remote_conn(sec, link, theirs, token).await;
        }
        let mut addrs = theirs.addrs.clone();
        // Where this peer can be reached: the URL we dialed, or its address as seen here.
        let reach = url.clone().or_else(|| match (ip, theirs.listen_port) {
            (Some(IpAddr::V6(v6)), Some(port)) => Some(format!("ws://[{v6}]:{port}")),
            (Some(IpAddr::V4(v4)), Some(port)) => Some(format!("ws://{v4}:{port}")),
            _ => None,
        });
        addrs.extend(reach.clone());
        let noise_pub = hex::encode(sec.remote_static);
        let my_mesh = self.mesh_id();

        // Who is this, relative to us?
        if let Some(mesh_id) = my_mesh.clone() {
            let known = members::get(&self.db, &theirs.node_id)?.filter(|m| m.mesh_id == mesh_id);
            if known.as_ref().is_some_and(Member::is_remote) {
                bail!("{} is a phone and can't connect as a full member", theirs.name);
            }
            let trusted = known
                .as_ref()
                .is_some_and(|m| !m.is_removed() && m.noise_pub == noise_pub && m.sign_pub == theirs.sign_pub);
            if !trusted {
                if theirs.joining {
                    // New, or coming back after being removed: a member has to approve it. Its
                    // address counts only once it is a member (dial_round dials members only),
                    // but other members and phones learn it from here.
                    self.learn_addrs(&theirs.node_id, addrs);
                    return self.pending_join(sec, link, theirs, noise_pub, token).await;
                }
                if let Some(m) = &known {
                    if m.is_removed() {
                        bail!("{} was removed from the mesh", m.name);
                    }
                    bail!("{} presented different keys than its admission record", m.name);
                }
                // A member admitted elsewhere whose record hasn't reached us yet.
                let rec = theirs.record.clone().filter(|r| {
                    r.node_id == theirs.node_id
                        && r.mesh_id == mesh_id
                        && r.noise_pub == noise_pub
                        && !r.is_removed()
                        && !r.is_remote()
                });
                match rec {
                    Some(r) if members::verify_record(&self.db, &r)? => members::upsert(&self.db, &r)?,
                    Some(_) => bail!("can't verify the admission of {} yet", theirs.name),
                    None => bail!("{} is not a member of this mesh", theirs.name),
                }
            }
            self.learn_addrs(&theirs.node_id, addrs);
            let opener = if initiator { self.me.clone() } else { theirs.node_id.clone() };
            if theirs.data_key_gen < self.data_key().map(|d| d.generation).unwrap_or(0)
                && let Some(dk) = self.data_key()
            {
                send(&mut sec, &link, &Msg::DataKey { key: dk.encode() })?;
            }
            return self.session(sec, link, theirs.node_id, theirs.name, opener, via, reach, token).await;
        }

        // We are joining: wait for a member to admit us.
        if self.st.lock().unwrap().joining.is_none() || theirs.mesh_id.is_none() {
            bail!("not in a mesh");
        }
        let code = handshake::confirmation_code(&self.ident.noise_public, &sec.remote_static);
        self.update_join(|j| {
            j.code = Some(code.clone());
            j.status = "waiting".into();
            j.error = None;
        });
        loop {
            let msg = tokio::select! {
                _ = token.cancelled() => bail!("stopped"),
                m = recv(&mut sec, &mut link) => m?,
            };
            match msg {
                None => {
                    self.update_join(|j| j.status = "connecting".into());
                    bail!("connection closed while waiting for approval");
                }
                Some(Msg::JoinPending { .. }) => {}
                Some(Msg::Rejected { reason }) => {
                    self.update_join(|j| {
                        j.status = "rejected".into();
                        j.error = Some(reason.clone());
                    });
                    bail!("rejected: {reason}");
                }
                Some(Msg::Admitted { members: recs, data_key }) => {
                    let mine = recs.iter().find(|m| m.node_id == self.me).cloned();
                    let ok = mine.as_ref().is_some_and(|m| {
                        m.noise_pub == self.ident.noise_pub_hex() && m.sign_pub == self.ident.sign_pub_hex()
                    });
                    if !ok {
                        bail!("admission record doesn't match this node");
                    }
                    for r in &recs {
                        members::upsert(&self.db, r)?;
                    }
                    let pass = self.st.lock().unwrap().joining.take().map(|j| j.passphrase).unwrap_or_default();
                    self.keys.set(PASSPHRASE, &pass);
                    if let Some(dk) = DataKey::decode(&data_key) {
                        self.keys.set(DATA_KEY, &dk.encode());
                        secrets::reseal(&self.db, self.keys.as_ref(), &dk)?;
                    }
                    self.learn_addrs(&theirs.node_id, addrs);
                    self.emit_status();
                    let opener = self.me.clone();
                    return self.session(sec, link, theirs.node_id, theirs.name, opener, via, reach, token).await;
                }
                Some(_) => {}
            }
        }
    }

    /// Disconnect a removed member once the removal record has had a moment to reach it.
    fn drop_session_soon(&self, node: &str) {
        self.changed.send_modify(|v| *v += 1);
        let phone = {
            let mut st = self.st.lock().unwrap();
            st.tickets.retain(|_, t| t.node != node);
            st.remotes.get(node).map(|r| (r.tx.clone(), r.kill.clone()))
        };
        // Records don't sync to phones, so say it outright.
        if let Some((tx, kill)) = phone {
            let _ = tx.send(Msg::Rejected { reason: "this device was removed from the mesh".into() });
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                kill.cancel();
            });
        }
        let kill = self.st.lock().unwrap().sessions.get(node).map(|s| s.kill.clone());
        if let Some(kill) = kill {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                kill.cancel();
            });
        }
    }

    fn update_join(&self, f: impl FnOnce(&mut Joining)) {
        if let Some(j) = self.st.lock().unwrap().joining.as_mut() {
            f(j);
        }
        self.emit_status();
    }

    fn set_join_error(&self, url: Option<&str>, msg: &str) {
        let mut st = self.st.lock().unwrap();
        if let (Some(j), Some(u)) = (st.joining.as_mut(), url)
            && j.url == u
        {
            j.error = Some(msg.to_owned());
            j.status = "failed".into();
            drop(st);
            self.emit_status();
        }
    }

    fn hello(&self) -> Hello {
        let rec = self.my_record().filter(|m| !m.is_removed());
        let joining = self.st.lock().unwrap().joining.is_some();
        Hello {
            node_id: self.me.clone(),
            name: self.name(),
            sign_pub: self.ident.sign_pub_hex(),
            version: PROTOCOL_VERSION,
            mesh_id: rec.as_ref().map(|r| r.mesh_id.clone()),
            record: rec,
            joining: joining && self.mesh_id().is_none(),
            data_key_gen: self.data_key().map(|d| d.generation).unwrap_or(0),
            addrs: self.advertise(),
            listen_port: *self.bound_port.lock().unwrap(),
            role: None,
            features: vec![FEATURE_GATEWAY.into()],
        }
    }

    /// A node asking to join: park it until the user approves or declines.
    async fn pending_join(
        self: Arc<Self>,
        mut sec: Secure,
        mut link: Link,
        theirs: Hello,
        noise_pub: String,
        token: CancellationToken,
    ) -> Result<()> {
        let code = handshake::confirmation_code(&self.ident.noise_public, &sec.remote_static);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let role = theirs.role.clone().filter(|r| r == ROLE_REMOTE);
        self.st.lock().unwrap().pending.insert(
            theirs.node_id.clone(),
            Pending {
                name: theirs.name.clone(),
                code: code.clone(),
                sign_pub: theirs.sign_pub.clone(),
                noise_pub,
                role: role.clone(),
                tx,
            },
        );
        self.emit_status();
        self.ui("mesh://join-request", json!({ "nodeId": theirs.node_id, "name": theirs.name, "code": code, "role": role }));
        send(&mut sec, &link, &Msg::JoinPending { code })?;
        let deadline = tokio::time::sleep(Duration::from_secs(600));
        tokio::pin!(deadline);
        let outcome = loop {
            tokio::select! {
                _ = token.cancelled() => break None,
                _ = &mut deadline => break None,
                m = recv(&mut sec, &mut link) => match m {
                    Ok(Some(_)) => {}
                    _ => break None,
                },
                out = rx.recv() => match out {
                    Some(m @ Msg::Admitted { .. }) => {
                        send(&mut sec, &link, &m)?;
                        break Some(());
                    }
                    Some(m) => {
                        let _ = send(&mut sec, &link, &m);
                        break None;
                    }
                    None => break None,
                },
            }
        };
        self.st.lock().unwrap().pending.remove(&theirs.node_id);
        self.emit_status();
        match outcome {
            Some(()) if role.is_some() => self.remote_session(sec, link, theirs.node_id, theirs.name, token).await,
            Some(()) => {
                let opener = theirs.node_id.clone();
                self.session(sec, link, theirs.node_id, theirs.name, opener, None, None, token).await
            }
            None => Ok(()),
        }
    }

    fn approve(&self, node_id: &str) -> Result<()> {
        let mesh_id = self.mesh_id().ok_or_else(|| anyhow!("not in a mesh"))?;
        let (name, sign_pub, noise_pub, role, tx) = {
            let st = self.st.lock().unwrap();
            let p = st.pending.get(node_id).ok_or_else(|| anyhow!("that request is gone"))?;
            (p.name.clone(), p.sign_pub.clone(), p.noise_pub.clone(), p.role.clone(), p.tx.clone())
        };
        let rec = Member::admit(&self.me, &self.ident, &mesh_id, node_id, &name, &sign_pub, &noise_pub, role.as_deref());
        members::upsert(&self.db, &rec)?;
        // A phone gets no data key: it holds no secrets and syncs nothing.
        let data_key = match role {
            Some(_) => String::new(),
            None => self.data_key().ok_or_else(|| anyhow!("no mesh data key"))?.encode(),
        };
        let recs: Vec<Member> = members::list(&self.db)?.into_iter().filter(|m| m.mesh_id == mesh_id).collect();
        tx.send(Msg::Admitted { members: recs, data_key }).map_err(|_| anyhow!("the node disconnected"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn session(
        self: Arc<Self>,
        mut sec: Secure,
        mut link: Link,
        node: String,
        name: String,
        opener: String,
        via: Option<String>,
        url: Option<String>,
        token: CancellationToken,
    ) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kill = token.child_token();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = self.st.lock().unwrap();
            let keep_new = match st.sessions.get(&node) {
                None => true,
                Some(old) if old.via.is_some() && via.is_none() => true,
                Some(old) if old.via.is_none() && via.is_some() => false,
                Some(old) => {
                    let preferred = std::cmp::min(self.me.clone(), node.clone());
                    // Two connections opened at once: keep the one the lower id opened. A
                    // reconnect from the same side replaces the stale one.
                    old.opener != preferred || opener == preferred
                }
            };
            if !keep_new {
                return Ok(());
            }
            if let Some(old) = st.sessions.insert(
                node.clone(),
                Sess { id, tx: tx.clone(), kill: kill.clone(), via: via.clone(), url, rtt: None, opener },
            ) {
                old.kill.cancel();
            }
            for s in st.streams.values_mut().filter(|s| s.streamer == node) {
                s.lost_at = None;
            }
        }
        tracing::info!("mesh: connected to {name}{}", via.as_ref().map(|v| format!(" via {v}")).unwrap_or_default());
        self.emit_status();
        self.send_links();

        let db = self.db.clone();
        let my_vv = tokio::task::spawn_blocking(move || db.with_conn(|c| sync::version_vector(c))).await??;
        send(&mut sec, &link, &Msg::SyncVv { vv: my_vv })?;
        for r in self.st.lock().unwrap().links.values() {
            let _ = tx.send(Msg::Links(r.clone()));
        }
        for p in self.own_stream_snapshots() {
            send(&mut sec, &link, &Msg::Event { name: "mesh://chat".into(), payload: p })?;
        }

        let mut peer_vv: Option<VersionVector> = None;
        let mut changed = self.changed.subscribe();
        let mut ping = tokio::time::interval(Duration::from_secs(5));
        let mut last_seen = Instant::now();
        let res: Result<()> = async {
            loop {
                tokio::select! {
                    _ = kill.cancelled() => return Ok(()),
                    m = recv(&mut sec, &mut link) => {
                        let Some(m) = m? else { return Ok(()) };
                        last_seen = Instant::now();
                        match m {
                            Msg::SyncVv { vv } => {
                                peer_vv = Some(vv);
                                self.push_ops(&mut sec, &link, peer_vv.as_mut().unwrap()).await?;
                            }
                            Msg::SyncOps { ops, upto } => {
                                if let Some(pv) = peer_vv.as_mut() {
                                    sync::merge(pv, &upto);
                                }
                                self.clone().apply_ops(ops, upto).await?;
                            }
                            Msg::Ping { t } => send(&mut sec, &link, &Msg::Pong { t })?,
                            Msg::Pong { t } => {
                                let rtt = (now_ms() - t).clamp(0, u32::MAX as i64) as u32;
                                if let Some(s) = self.st.lock().unwrap().sessions.get_mut(&node).filter(|s| s.id == id) {
                                    s.rtt = Some(rtt);
                                }
                            }
                            other => self.clone().handle(&node, other, &tx),
                        }
                    }
                    out = rx.recv() => {
                        let Some(out) = out else { return Ok(()) };
                        send(&mut sec, &link, &out)?;
                    }
                    r = changed.changed() => {
                        if r.is_err() { return Ok(()) }
                        if let Some(pv) = peer_vv.as_mut() {
                            self.push_ops(&mut sec, &link, pv).await?;
                        }
                    }
                    _ = ping.tick() => {
                        if last_seen.elapsed() > Duration::from_secs(30) {
                            bail!("peer stopped responding");
                        }
                        send(&mut sec, &link, &Msg::Ping { t: now_ms() })?;
                    }
                }
            }
        }
        .await;

        let removed = {
            let mut st = self.st.lock().unwrap();
            let mine = st.sessions.get(&node).is_some_and(|s| s.id == id);
            if mine {
                st.sessions.remove(&node);
                let now = now_ms();
                for s in st.streams.values_mut().filter(|s| s.streamer == node && !s.done) {
                    s.lost_at.get_or_insert(now);
                }
            }
            mine
        };
        if removed {
            tracing::info!("mesh: disconnected from {name}");
            self.emit_status();
            self.send_links();
        }
        res
    }

    /// Send a peer every change it lacks, in batches.
    async fn push_ops(&self, sec: &mut Secure, link: &Link, peer_vv: &mut VersionVector) -> Result<()> {
        loop {
            let db = self.db.clone();
            let vv = peer_vv.clone();
            // Both reads under one lock, so no write can land between them.
            let (mine, batch) = tokio::task::spawn_blocking(move || {
                db.with_conn(|c| -> Result<_> { Ok((sync::version_vector(c)?, sync::ops_since(c, &vv, SYNC_BATCH)?)) })
            })
            .await??;
            if batch.ops.is_empty() && !batch.more {
                // The peer lacks nothing that still matters (gaps are superseded changes):
                // adopt our view so the next check starts from here.
                for (o, s) in mine {
                    let e = peer_vv.entry(o).or_insert(0);
                    *e = (*e).max(s);
                }
                return Ok(());
            }
            sync::merge(peer_vv, &batch.upto);
            let more = batch.more;
            if !batch.upto.is_empty() {
                send(sec, link, &Msg::SyncOps { ops: batch.ops, upto: batch.upto })?;
            }
            if !more {
                return Ok(());
            }
        }
    }

    async fn apply_ops(self: Arc<Self>, ops: Vec<Op>, upto: VersionVector) -> Result<()> {
        let db = self.db.clone();
        let defaults = Defaults { data_dir: self.data_dir.clone() };
        let applied = tokio::task::spawn_blocking(move || {
            db.with_conn(|c| -> Result<Applied> {
                let a = sync::apply(c, &ops, &defaults)?;
                sync::record_upto(c, &upto)?;
                Ok(a)
            })
        })
        .await??;
        if applied.rows.is_empty() {
            return Ok(());
        }
        self.after_apply(&applied);
        // Other peers may lack these too.
        self.changed.send_modify(|v| *v += 1);
        Ok(())
    }

    fn after_apply(&self, applied: &Applied) {
        if applied.tables.contains("mesh_members") {
            let mut drop_nodes = Vec::new();
            for (t, pk) in &applied.rows {
                if t != "mesh_members" {
                    continue;
                }
                if let Ok(Some(m)) = members::get(&self.db, pk) {
                    match members::verify_record(&self.db, &m) {
                        Ok(_) => {}
                        Err(e) => tracing::warn!("mesh: {e:#}"),
                    }
                    if m.is_removed() {
                        drop_nodes.push(m.node_id.clone());
                    }
                }
            }
            for n in drop_nodes {
                if n == self.me {
                    tracing::warn!("mesh: this node was removed from the mesh");
                    if let Some(t) = self.run.lock().unwrap().take() {
                        t.cancel();
                    }
                    self.keys.delete(PASSPHRASE);
                    self.keys.delete(DATA_KEY);
                } else {
                    self.drop_session_soon(&n);
                }
            }
            self.emit_status();
        }
        if applied.tables.contains("mesh_secrets")
            && let Some(dk) = self.data_key()
        {
            for (t, pk) in &applied.rows {
                if t == "mesh_secrets"
                    && let Err(e) = secrets::install(&self.db, self.keys.as_ref(), &dk, pk)
                {
                    tracing::warn!("mesh: installing secret: {e:#}");
                }
            }
        }
        if let Some(h) = self.handler.read().unwrap().clone() {
            h.synced(applied);
        }
        let tables: Vec<&String> = applied.tables.iter().collect();
        self.ui("mesh://changed", json!({ "tables": tables }));
    }

    fn handle(self: Arc<Self>, from: &str, m: Msg, reply: &mpsc::UnboundedSender<Msg>) {
        match m {
            Msg::Event { name, payload } => {
                if name == "mesh://chat" {
                    self.track_stream(&payload);
                }
                self.fanout(&name, &payload);
                if let Some(h) = self.handler.read().unwrap().clone() {
                    h.event(&name, &payload);
                }
                self.ui(&name, payload);
            }
            Msg::Rpc { id, method, params } => {
                let reply = reply.clone();
                let from = from.to_owned();
                tokio::spawn(async move {
                    let r = self.clone().serve_rpc(from, method, params).await;
                    let msg = match r {
                        Ok(v) => Msg::RpcResult { id, ok: Some(v), err: None },
                        Err(e) => Msg::RpcResult { id, ok: None, err: Some(format!("{e:#}")) },
                    };
                    let _ = reply.send(msg);
                });
            }
            Msg::RpcResult { id, ok, err } => {
                if let Some(w) = self.rpc_waiters.lock().unwrap().remove(&id) {
                    let _ = w.send(match err {
                        Some(e) => Err(e),
                        None => Ok(ok.unwrap_or(Value::Null)),
                    });
                }
            }
            Msg::Links(r) => {
                let newer = {
                    let mut st = self.st.lock().unwrap();
                    let newer = r.origin != self.me && st.links.get(&r.origin).is_none_or(|old| old.ver < r.ver);
                    if newer {
                        st.links.insert(r.origin.clone(), r.clone());
                    }
                    newer
                };
                if newer {
                    self.learn_addrs(&r.origin, r.addrs.clone());
                    for (n, u) in r.peers.iter().filter(|(n, _)| *n != self.me) {
                        self.learn_addrs(n, [u.clone()]);
                    }
                    // Flood onwards.
                    let st = self.st.lock().unwrap();
                    for (n, s) in &st.sessions {
                        if n != from {
                            let _ = s.tx.send(Msg::Links(r.clone()));
                        }
                    }
                }
            }
            Msg::DataKey { key } => {
                if let Some(dk) = DataKey::decode(&key)
                    && let Err(e) = self.adopt_data_key(dk)
                {
                    tracing::warn!("mesh: data key: {e:#}");
                }
            }
            Msg::Relay { circuit, from: src, to, ttl, data, close } => self.relay(from, circuit, src, to, ttl, data, close),
            _ => {}
        }
    }

    // ---------------- RPC ----------------

    async fn rpc(&self, node: &str, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let tx = self
            .st
            .lock()
            .unwrap()
            .sessions
            .get(node)
            .map(|s| s.tx.clone())
            .ok_or_else(|| anyhow!("{} is offline", self.member_name(node)))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (wtx, wrx) = oneshot::channel();
        self.rpc_waiters.lock().unwrap().insert(id, wtx);
        tx.send(Msg::Rpc { id, method: method.to_owned(), params })
            .map_err(|_| anyhow!("{} disconnected", self.member_name(node)))?;
        match tokio::time::timeout(timeout, wrx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => Err(anyhow!("{} disconnected", self.member_name(node))),
            Err(_) => {
                self.rpc_waiters.lock().unwrap().remove(&id);
                Err(anyhow!("{} did not answer in time", self.member_name(node)))
            }
        }
    }

    async fn serve_rpc(self: Arc<Self>, from: String, method: String, params: Value) -> Result<Value> {
        match method.as_str() {
            "mesh.file" => {
                let sha = params["sha"].as_str().unwrap_or_default();
                let name = params["name"].as_str().unwrap_or_default();
                if !is_sha256(sha) {
                    bail!("bad file id");
                }
                let offset = params["offset"].as_u64().unwrap_or(0);
                let path = crate::attachments::stored_path(&self.data_dir, sha, name);
                read_chunk(path, offset).await
            }
            _ => {
                let h = self.handler.read().unwrap().clone().ok_or_else(|| anyhow!("not ready"))?;
                h.call(from, method, params).await
            }
        }
    }

    async fn ensure_file(self: Arc<Self>, sha: &str, name: &str) -> Result<PathBuf> {
        if !is_sha256(sha) {
            bail!("bad file id");
        }
        let path = crate::attachments::stored_path(&self.data_dir, sha, name);
        if path.exists() {
            return Ok(path);
        }
        let peers: Vec<String> = self.st.lock().unwrap().sessions.keys().cloned().collect();
        for p in peers {
            match self.fetch_file(&p, sha, name, &path).await {
                Ok(()) => return Ok(path),
                Err(e) => tracing::debug!("mesh: fetching {name} from {p}: {e:#}"),
            }
        }
        bail!("{name} isn't on this computer and no connected node has it")
    }

    async fn fetch_file(&self, node: &str, sha: &str, name: &str, dest: &std::path::Path) -> Result<()> {
        use sha2::{Digest, Sha256};
        let mut data = Vec::new();
        loop {
            let r = self.rpc(node, "mesh.file", json!({ "sha": sha, "name": name, "offset": data.len() }), RPC_TIMEOUT).await?;
            let chunk = B64.decode(r["data"].as_str().unwrap_or_default())?;
            data.extend_from_slice(&chunk);
            if r["eof"].as_bool().unwrap_or(true) || chunk.is_empty() {
                break;
            }
        }
        if hex::encode(Sha256::digest(&data)) != sha {
            bail!("checksum mismatch");
        }
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = dest.with_extension("part");
        tokio::fs::write(&tmp, &data).await?;
        tokio::fs::rename(&tmp, dest).await?;
        Ok(())
    }

    fn broadcast(&self, m: Msg) {
        for s in self.st.lock().unwrap().sessions.values() {
            let _ = s.tx.send(m.clone());
        }
    }

    // ---------------- links & relaying ----------------

    fn send_links(&self) {
        let report = {
            let st = self.st.lock().unwrap();
            LinkReport {
                origin: self.me.clone(),
                ver: now_ms(),
                links: st
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.via.is_none())
                    .map(|(n, s)| (n.clone(), s.rtt.unwrap_or(50)))
                    .collect(),
                addrs: self.advertise(),
                peers: st
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.via.is_none())
                    .filter_map(|(n, s)| s.url.clone().map(|u| (n.clone(), u)))
                    .collect(),
            }
        };
        self.broadcast(Msg::Links(report));
    }

    /// First hop toward `target`: itself when directly connected, else the start of the
    /// lowest-latency path through the nodes' reported links.
    fn next_hop(&self, target: &str) -> Option<String> {
        let st = self.st.lock().unwrap();
        if st.sessions.get(target).is_some_and(|s| s.via.is_none()) {
            return Some(target.to_owned());
        }
        // Dijkstra over a handful of nodes.
        let mut dist: HashMap<String, (u64, String)> = HashMap::new();
        let mut queue: Vec<(u64, String, String)> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.via.is_none())
            .map(|(n, s)| (s.rtt.unwrap_or(50) as u64, n.clone(), n.clone()))
            .collect();
        while let Some(i) = queue.iter().enumerate().min_by_key(|(_, q)| q.0).map(|(i, _)| i) {
            let (d, node, first) = queue.swap_remove(i);
            if dist.contains_key(&node) || node == self.me {
                continue;
            }
            dist.insert(node.clone(), (d, first.clone()));
            if node == target {
                return Some(first);
            }
            if let Some(r) = st.links.get(&node) {
                for (peer, rtt) in &r.links {
                    if !dist.contains_key(peer) {
                        queue.push((d + *rtt as u64, peer.clone(), first.clone()));
                    }
                }
            }
        }
        None
    }

    /// A connection to `target` tunnelled through other nodes.
    fn circuit_link(self: Arc<Self>, circuit: u64, target: String) -> Link {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        self.st.lock().unwrap().circuits.insert(circuit, in_tx);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let inner = self.clone();
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                inner.route(Msg::Relay {
                    circuit,
                    from: inner.me.clone(),
                    to: target.clone(),
                    ttl: 8,
                    data: Some(B64.encode(frame)),
                    close: false,
                });
            }
            inner.st.lock().unwrap().circuits.remove(&circuit);
            inner.route(Msg::Relay { circuit, from: inner.me.clone(), to: target, ttl: 8, data: None, close: true });
        });
        Link { tx: out_tx, rx: in_rx }
    }

    fn route(&self, m: Msg) {
        let Msg::Relay { to, .. } = &m else { return };
        let Some(hop) = self.next_hop(to) else { return };
        if let Some(s) = self.st.lock().unwrap().sessions.get(&hop) {
            let _ = s.tx.send(m);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn relay(self: Arc<Self>, prev: &str, circuit: u64, src: String, to: String, ttl: u8, data: Option<String>, close: bool) {
        if to != self.me {
            if ttl > 0 {
                self.route(Msg::Relay { circuit, from: src, to, ttl: ttl - 1, data, close });
            }
            return;
        }
        if close {
            self.st.lock().unwrap().circuits.remove(&circuit);
            return;
        }
        let Some(frame) = data.and_then(|d| B64.decode(d).ok()) else { return };
        let existing = self.st.lock().unwrap().circuits.get(&circuit).cloned();
        match existing {
            Some(tx) => {
                let _ = tx.send(frame);
            }
            None => {
                // Someone is opening a connection to us through `prev`.
                let link = self.clone().circuit_link(circuit, src);
                if let Some(tx) = self.st.lock().unwrap().circuits.get(&circuit) {
                    let _ = tx.send(frame);
                }
                let token = match self.run.lock().unwrap().clone() {
                    Some(t) => t,
                    None => return,
                };
                let via = prev.to_owned();
                let inner = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = inner.run_conn(link, false, None, None, Some(via), token).await {
                        tracing::debug!("mesh: relayed inbound: {e:#}");
                    }
                });
            }
        }
    }

    // ---------------- remote members (phones) ----------------

    fn ticket_key(&self, id: &[u8; TICKET_ID_LEN]) -> Option<[u8; 32]> {
        let st = self.st.lock().unwrap();
        st.tickets.get(id).filter(|t| t.expires > now_ms()).map(|t| t.key)
    }

    fn issue_ticket(&self, node: &str, noise_pub: [u8; 32]) -> Msg {
        let id: [u8; TICKET_ID_LEN] = rand::random();
        let key = random32();
        let expires = now_ms() + TICKET_MS;
        let mut st = self.st.lock().unwrap();
        // Keep the newest few per device.
        let mut mine: Vec<([u8; TICKET_ID_LEN], i64)> =
            st.tickets.iter().filter(|(_, t)| t.node == node).map(|(k, t)| (*k, t.expires)).collect();
        mine.sort_by_key(|x| x.1);
        while mine.len() >= 4 {
            let (k, _) = mine.remove(0);
            st.tickets.remove(&k);
        }
        st.tickets.insert(id, Ticket { node: node.to_owned(), noise_pub, key, expires });
        Msg::Ticket { id: hex::encode(id), key: hex::encode(key), expires }
    }

    /// A phone connecting: check it against its record (or park it for approval), then serve it.
    async fn remote_conn(self: Arc<Self>, mut sec: Secure, link: Link, theirs: Hello, token: CancellationToken) -> Result<()> {
        let Some(mesh_id) = self.mesh_id() else { bail!("not in a mesh") };
        let noise_pub = hex::encode(sec.remote_static);
        let known = members::get(&self.db, &theirs.node_id)?.filter(|m| m.mesh_id == mesh_id);
        let trusted = known.as_ref().is_some_and(|m| {
            !m.is_removed() && m.is_remote() && m.noise_pub == noise_pub && m.sign_pub == theirs.sign_pub
        });
        if !trusted {
            if theirs.joining {
                return self.pending_join(sec, link, theirs, noise_pub, token).await;
            }
            if let Some(m) = &known {
                if m.is_removed() {
                    let _ = send(&mut sec, &link, &Msg::Rejected { reason: "this device was removed from the mesh".into() });
                    bail!("{} was removed from the mesh", m.name);
                }
                bail!("{} presented different keys than its admission record", m.name);
            }
            let rec = theirs.record.clone().filter(|r| {
                r.node_id == theirs.node_id && r.mesh_id == mesh_id && r.noise_pub == noise_pub && r.is_remote() && !r.is_removed()
            });
            match rec {
                Some(r) if members::verify_record(&self.db, &r)? => members::upsert(&self.db, &r)?,
                Some(_) => bail!("can't verify the admission of {} yet", theirs.name),
                None => bail!("{} is not a member of this mesh", theirs.name),
            }
        }
        self.remote_session(sec, link, theirs.node_id, theirs.name, token).await
    }

    /// Serve a phone that uses this node as its gateway: its requests, the topics it follows,
    /// and a nudge whenever the lists it shows may have changed.
    async fn remote_session(
        self: Arc<Self>,
        mut sec: Secure,
        mut link: Link,
        node: String,
        name: String,
        token: CancellationToken,
    ) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kill = token.child_token();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let old = self.st.lock().unwrap().remotes.insert(
            node.clone(),
            RemoteSess { id, tx: tx.clone(), kill: kill.clone(), subs: HashSet::new() },
        );
        if let Some(old) = old {
            old.kill.cancel();
        }
        tracing::info!("mesh: phone {name} connected");
        self.emit_status();
        let ticket = self.issue_ticket(&node, sec.remote_static);
        send(&mut sec, &link, &ticket)?;
        let mut changed = self.changed.subscribe();
        let mut index_due: Option<tokio::time::Instant> = None;
        let mut last_seen = Instant::now();
        let mut check = tokio::time::interval(Duration::from_secs(10));
        let res: Result<()> = async {
            loop {
                let due = async {
                    match index_due {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = kill.cancelled() => return Ok(()),
                    m = recv(&mut sec, &mut link) => {
                        let Some(m) = m? else { return Ok(()) };
                        last_seen = Instant::now();
                        match m {
                            Msg::Rpc { id, method, params } => self.clone().remote_rpc(&node, id, method, params, tx.clone()),
                            Msg::Ping { t } => send(&mut sec, &link, &Msg::Pong { t })?,
                            _ => {}
                        }
                    }
                    out = rx.recv() => {
                        let Some(out) = out else { return Ok(()) };
                        send(&mut sec, &link, &out)?;
                    }
                    r = changed.changed() => {
                        if r.is_err() {
                            return Ok(());
                        }
                        // Coalesced: an agent's turn writes often, and each nudge refetches lists.
                        index_due.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(1500));
                    }
                    _ = due => {
                        index_due = None;
                        let follows = self.st.lock().unwrap().remotes.get(&node).is_some_and(|r| r.id == id && r.subs.contains(rmt::TOPIC_INDEX));
                        if follows {
                            send(&mut sec, &link, &Msg::Event { name: rmt::EV_INDEX.into(), payload: Value::Null })?;
                        }
                    }
                    _ = check.tick() => {
                        if last_seen.elapsed() > REMOTE_TIMEOUT {
                            bail!("{name} stopped responding");
                        }
                    }
                }
            }
        }
        .await;
        let mine = {
            let mut st = self.st.lock().unwrap();
            let mine = st.remotes.get(&node).is_some_and(|r| r.id == id);
            if mine {
                st.remotes.remove(&node);
            }
            mine
        };
        if mine {
            tracing::info!("mesh: phone {name} disconnected");
            self.emit_status();
        }
        res
    }

    fn remote_rpc(self: Arc<Self>, from: &str, id: u64, method: String, params: Value, reply: mpsc::UnboundedSender<Msg>) {
        let from = from.to_owned();
        // Register the topics now, so events after this point follow the reply in order.
        let plans = (method == "remote.subscribe").then(|| self.plan_subscribe(&from, &params));
        tokio::spawn(async move {
            let r = match method.as_str() {
                "remote.subscribe" => Ok(self.clone().finish_subscribe(&from, plans.unwrap_or_default()).await),
                "remote.unsubscribe" => {
                    let topics: Vec<String> = serde_json::from_value(params["topics"].clone()).unwrap_or_default();
                    if let Some(r) = self.st.lock().unwrap().remotes.get_mut(&from) {
                        for t in topics {
                            r.subs.remove(&t);
                        }
                    }
                    Ok(Value::Null)
                }
                "remote.members" => Ok(Mesh(self.clone()).remote_members()),
                "mesh.file" => self.clone().serve_rpc(from.clone(), method.clone(), params).await,
                m if m.starts_with("remote.") => {
                    let h = self.handler.read().unwrap().clone();
                    match h {
                        Some(h) => h.call(from, method, params).await,
                        None => Err(anyhow!("not ready")),
                    }
                }
                other => Err(anyhow!("{other} isn't available to a phone")),
            };
            let msg = match r {
                Ok(v) => Msg::RpcResult { id, ok: Some(v), err: None },
                Err(e) => Msg::RpcResult { id, ok: None, err: Some(format!("{e:#}")) },
            };
            let _ = reply.send(msg);
        });
    }

    /// Start following topics. A known point still in the ring resumes with a delta; anything
    /// else gets a snapshot (finished by [`finish_subscribe`] with the app's data).
    fn plan_subscribe(&self, from: &str, params: &Value) -> Vec<Plan> {
        let reqs = params["topics"].as_array().cloned().unwrap_or_default();
        let mut st = self.st.lock().unwrap();
        let mut plans = Vec::new();
        for r in reqs {
            let Some(topic) = r["topic"].as_str().map(str::to_owned) else { continue };
            if let Some(rs) = st.remotes.get_mut(from) {
                rs.subs.insert(topic.clone());
            }
            let point: Option<rmt::Point> = serde_json::from_value(r["point"].clone()).ok();
            let ring = st.rings.get(&topic).map(|(r, _)| r);
            let last = ring.and_then(Ring::last);
            if let (Some(p), Some(ring)) = (&point, ring)
                && let Some(events) = ring.since(p)
            {
                plans.push(Plan { topic, delta: Some(events), tail: Vec::new(), covered: true, point: last.or(point) });
                continue;
            }
            let (tail, covered) = match ring {
                Some(ring) if topic.starts_with("session:") => {
                    ring.tail(|_, p| matches!(p["event"]["type"].as_str(), Some("promptDone" | "sessionReady")))
                }
                _ => (Vec::new(), true),
            };
            plans.push(Plan { topic, delta: None, tail, covered, point: last });
        }
        plans
    }

    async fn finish_subscribe(self: Arc<Self>, from: &str, plans: Vec<Plan>) -> Value {
        let mut results = Vec::new();
        for p in plans {
            let pair = |(n, v): (String, Value)| json!([n, v]);
            if let Some(events) = p.delta {
                let events: Vec<Value> = rmt::coalesce(events).into_iter().map(pair).collect();
                results.push(json!({ "topic": p.topic, "mode": "delta", "events": events, "point": p.point }));
                continue;
            }
            let mut snapshot = Value::Null;
            let mut tail = p.tail;
            if let Some(id) = p.topic.strip_prefix("session:") {
                let h = self.handler.read().unwrap().clone();
                if let Some(h) = h {
                    snapshot = h.call(from.to_owned(), "remote.snapshot".into(), json!({ "id": id })).await.unwrap_or(Value::Null);
                }
                // The attach replay already brings the ready state, usage and open requests.
                let open: HashSet<String> = snapshot["attach"]["events"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|e| e["requestId"].as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                tail.retain(|(_, v)| {
                    let ev = &v["event"];
                    !matches!(ev["type"].as_str(), Some("sessionReady" | "usage"))
                        && !(ev["type"] == "permission" && ev["requestId"].as_str().is_some_and(|r| open.contains(r)))
                });
                for (_, v) in tail.iter_mut() {
                    if v["event"]["type"] == "userPrompt" {
                        v["event"]["replay"] = Value::Bool(true);
                    }
                }
            } else if let Some(conv) = p.topic.strip_prefix("chat:") {
                let st = self.st.lock().unwrap();
                snapshot = json!({ "stream": st.streams.get(conv).filter(|s| !s.done) });
            }
            let tail: Vec<Value> = rmt::coalesce(tail).into_iter().map(pair).collect();
            results.push(json!({
                "topic": p.topic,
                "mode": "snapshot",
                "snapshot": snapshot,
                "tail": tail,
                "covered": p.covered,
                "point": p.point,
            }));
        }
        json!({ "results": results })
    }

    /// Record a live event for resuming, update the session list's summary, and pass it to the
    /// phones following its topic.
    fn fanout(&self, name: &str, payload: &Value) {
        let Some(topic) = rmt::topic_of(name, payload) else { return };
        let mut st = self.st.lock().unwrap();
        if let Some(pt) = rmt::point_of(payload) {
            let e = st.rings.entry(topic.clone()).or_insert_with(|| (Ring::default(), Instant::now()));
            e.0.push(&pt, name, payload);
            e.1 = Instant::now();
        }
        let mut live = None;
        if name == rmt::EV_CODE
            && let Some(id) = payload["codeSessionId"].as_str()
        {
            let l = st.live.entry(id.to_owned()).or_default();
            if l.apply(payload, now_ms()) {
                live = Some(json!({ "id": id, "live": l }));
            }
        }
        if st.remotes.is_empty() {
            return;
        }
        for r in st.remotes.values() {
            if r.subs.contains(&topic) {
                let _ = r.tx.send(Msg::Event { name: name.to_owned(), payload: payload.clone() });
            }
            if let Some(l) = &live
                && r.subs.contains(rmt::TOPIC_INDEX)
            {
                let _ = r.tx.send(Msg::Event { name: rmt::EV_LIVE.into(), payload: l.clone() });
            }
        }
    }

    // ---------------- chat streams ----------------

    fn track_stream(&self, payload: &Value) {
        let Some(conv) = payload["conversationId"].as_str() else { return };
        let ev = &payload["event"];
        let mut st = self.st.lock().unwrap();
        let s = st.streams.entry(conv.to_owned()).or_insert_with(|| Stream {
            conversation_id: conv.to_owned(),
            streamer: payload["streamer"].as_str().unwrap_or_default().to_owned(),
            streamer_name: payload["streamerName"].as_str().unwrap_or_default().to_owned(),
            user_message_id: None,
            assistant_message_id: None,
            text: String::new(),
            reasoning: String::new(),
            status: None,
            done: false,
            lost_at: None,
        });
        match ev["type"].as_str().unwrap_or_default() {
            "started" | "snapshot" => {
                s.streamer = payload["streamer"].as_str().unwrap_or_default().to_owned();
                s.streamer_name = payload["streamerName"].as_str().unwrap_or_default().to_owned();
                s.user_message_id = ev["userMessageId"].as_str().map(str::to_owned);
                s.assistant_message_id = ev["assistantMessageId"].as_str().map(str::to_owned);
                s.text = ev["text"].as_str().unwrap_or_default().to_owned();
                s.reasoning = ev["reasoning"].as_str().unwrap_or_default().to_owned();
                s.done = false;
                s.lost_at = None;
            }
            "text" => s.text.push_str(ev["text"].as_str().unwrap_or_default()),
            "reasoning" => s.reasoning.push_str(ev["text"].as_str().unwrap_or_default()),
            "status" => s.status = ev["message"].as_str().map(str::to_owned),
            "done" | "error" => s.done = true,
            _ => {}
        }
        if s.done {
            st.streams.remove(conv);
        }
    }

    fn own_stream_snapshots(&self) -> Vec<Value> {
        let st = self.st.lock().unwrap();
        st.streams
            .values()
            .filter(|s| s.streamer == self.me && !s.done)
            .map(|s| {
                json!({
                    "conversationId": s.conversation_id,
                    "streamer": s.streamer,
                    "streamerName": s.streamer_name,
                    "event": {
                        "type": "snapshot",
                        "userMessageId": s.user_message_id,
                        "assistantMessageId": s.assistant_message_id,
                        "text": s.text,
                        "reasoning": s.reasoning,
                    },
                })
            })
            .collect()
    }

    /// Save replies whose streamer vanished mid-stream, with what arrived so far.
    fn finish_lost_streams(&self) {
        let now = now_ms();
        let lost: Vec<Stream> = {
            let mut st = self.st.lock().unwrap();
            let ids: Vec<String> = st
                .streams
                .iter()
                .filter(|(_, s)| !s.done && s.lost_at.is_some_and(|t| now - t > STREAM_GRACE_MS))
                .map(|(k, _)| k.clone())
                .collect();
            ids.iter().filter_map(|k| st.streams.remove(k)).collect()
        };
        for s in lost {
            let Some(aid) = &s.assistant_message_id else { continue };
            let msg = format!("Interrupted: {} went offline while replying", s.streamer_name);
            let reasoning = (!s.reasoning.is_empty()).then_some(s.reasoning.as_str());
            if let Err(e) = self.db.finish_message(aid, &s.text, reasoning, Some(&msg), None) {
                tracing::warn!("mesh: saving interrupted reply: {e:#}");
            }
            let base = json!({ "conversationId": s.conversation_id, "streamer": s.streamer, "streamerName": s.streamer_name });
            for ev in [json!({ "type": "error", "message": msg }), json!({ "type": "done", "stopReason": "interrupted", "usage": null })] {
                let mut p = base.clone();
                p["event"] = ev;
                self.ui("mesh://chat", p);
            }
        }
    }
}

/// Attachment files are named by their SHA-256; anything else could point outside the store.
fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn read_chunk(path: PathBuf, offset: u64) -> Result<Value> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    const CHUNK: usize = 4 * 1024 * 1024;
    let mut f = tokio::fs::File::open(&path).await.context("file not here")?;
    let size = f.metadata().await?.len();
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; CHUNK.min(size.saturating_sub(offset) as usize)];
    f.read_exact(&mut buf).await?;
    Ok(json!({ "data": B64.encode(&buf), "eof": offset + buf.len() as u64 >= size, "size": size }))
}

/// How to answer one topic of a phone's subscribe: with the events since its point, or with a
/// snapshot plus the events since the last finished turn.
struct Plan {
    topic: String,
    delta: Option<Vec<(String, Value)>>,
    tail: Vec<(String, Value)>,
    /// The tail starts where it should (false when the ring lost its beginning).
    covered: bool,
    point: Option<rmt::Point>,
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
