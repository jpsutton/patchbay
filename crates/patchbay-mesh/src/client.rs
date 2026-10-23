//! The remote member's side: join a mesh through any full member, then keep one connection to a
//! gateway, racing every known address whenever the network changes, resuming with a ticket
//! when it can, and resubscribing to live topics from the last point it applied.
//!
//! The client stores nothing but its identity, the member list, addresses and resume tickets.
//! Everything shown comes from the gateway.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::handshake::{self, HandshakeError, Link, Secure, TICKET_ID_LEN};
use crate::keys::{Identity, KeyStore, hex32, write_private};
use crate::member::{self, Member, ROLE_REMOTE};
use crate::remote::{self, Point};
use crate::wire::{EV_JOIN_REQUEST, EV_JOIN_RESOLVED, FEATURE_GATEWAY, Hello, JoinRequest, Msg, PROTOCOL_VERSION};
use crate::ws::{self, normalize_url, recv, send};

/// Delivers an event to the app: (event name, payload).
pub type Sink = Arc<dyn Fn(&str, Value) + Send + Sync>;

const PASSPHRASE: &str = "remote:passphrase";
const IDENTITY: &str = "remote:identity";
const TICKETS: &str = "remote:tickets";
/// Sent to the app when the client's state changes; `status()` has the details.
pub const EV_STATUS: &str = "remote://status";
/// Sent to the app when a resubscribe after reconnecting found the topic's events gone: the
/// payload is the gateway's snapshot (`topic`, `snapshot`, `tail`, `covered`, `point`).
pub const EV_RESYNC: &str = "remote://resync";
/// Sent to the app when the member list (and who is online) changed.
pub const EV_MEMBERS: &str = "remote://members";

const DIAL_TIMEOUT: Duration = Duration::from_secs(8);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const STAGGER: Duration = Duration::from_millis(250);
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an RPC waits for a gateway before failing.
const CONNECT_WAIT: Duration = Duration::from_secs(20);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// After this long out of sight, Android has most likely frozen the app and its connection is
/// gone; coming back, a shorter probe decides sooner to dial again.
const STALE_AFTER: Duration = Duration::from_secs(20);
/// Enough for a mobile radio to wake and answer one ping; past it, dialing again is faster.
const RESUME_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_CANDIDATES: usize = 16;

/// Public state, saved as `remote.json`.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct Saved {
    node_id: String,
    name: String,
    mesh_id: Option<String>,
    members: Vec<Member>,
    /// Known URLs per member.
    addrs: BTreeMap<String, BTreeSet<String>>,
    /// Who each member reported online, as of the last refresh.
    #[serde(default)]
    online: BTreeMap<String, String>,
    /// Round trip to each gateway last time, for ordering the race.
    #[serde(default)]
    rtt: BTreeMap<String, u32>,
    last_gateway: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Ticket {
    id: String,
    key: String,
    expires: i64,
}

struct Conn {
    gateway: String,
    name: String,
    url: String,
    tx: mpsc::UnboundedSender<Msg>,
    kill: CancellationToken,
    probe: Arc<Notify>,
    rtt: Option<u32>,
    resumed: bool,
}

struct Joining {
    url: String,
    passphrase: String,
    code: Option<String>,
    status: String,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    conn: Option<Conn>,
    joining: Option<Joining>,
    /// Topics to keep subscribed, with the last point applied.
    subs: HashMap<String, Option<Point>>,
    /// Events that arrived for a topic while its subscribe call was in flight.
    pending: HashMap<String, Vec<(String, Value)>>,
    waiters: HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>,
    /// `offline`, `connecting` or `online` once a member; join progress lives in `joining`.
    phase: String,
    error: Option<String>,
    foreground: bool,
    /// When the app went out of sight.
    hidden_since: Option<Instant>,
    /// How long the next probe waits for an answer, when not the usual `PROBE_TIMEOUT`.
    probe_timeout: Option<Duration>,
    /// Nodes waiting to join, as the gateway reports them; this device may decide them.
    join_requests: HashMap<String, JoinRequest>,
}

/// How long to wait for the connection to answer when the app comes back after `hidden_for`.
fn resume_probe_timeout(hidden_for: Duration) -> Duration {
    if hidden_for >= STALE_AFTER { RESUME_PROBE_TIMEOUT } else { PROBE_TIMEOUT }
}

struct Established {
    sec: Secure,
    link: Link,
    gateway: String,
    name: String,
    url: String,
    resumed: bool,
}

#[derive(Clone)]
pub struct RemoteClient(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    keys: Arc<dyn KeyStore>,
    /// Loaded when the connection manager starts, not in `new`: on Android the key store calls
    /// into the main thread, where `new` may be running.
    ident: OnceLock<Identity>,
    /// The runtime `start` ran on.
    rt: OnceLock<tokio::runtime::Handle>,
    sink: Sink,
    saved: Mutex<Saved>,
    tickets: Mutex<HashMap<String, Ticket>>,
    st: Mutex<State>,
    kick: Notify,
    connected: watch::Sender<bool>,
    next_id: AtomicU64,
    started: AtomicBool,
    stop: CancellationToken,
}

impl RemoteClient {
    /// Load this client's saved state under `dir`. Its keys load when [`start`](Self::start)ed.
    pub fn new(dir: PathBuf, keys: Arc<dyn KeyStore>, sink: Sink) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut saved: Saved = std::fs::read_to_string(dir.join("remote.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if saved.node_id.is_empty() {
            saved.node_id = hex::encode(rand::random::<[u8; 16]>());
        }
        let (connected, _) = watch::channel(false);
        let inner = Inner {
            dir,
            keys,
            ident: OnceLock::new(),
            rt: OnceLock::new(),
            sink,
            saved: Mutex::new(saved),
            tickets: Mutex::default(),
            st: Mutex::new(State { phase: "offline".into(), foreground: true, ..State::default() }),
            kick: Notify::new(),
            connected,
            next_id: AtomicU64::new(1),
            started: AtomicBool::new(false),
            stop: CancellationToken::new(),
        };
        inner.save();
        let c = RemoteClient(Arc::new(inner));
        // The index topic carries list changes and live session summaries.
        c.0.st.lock().unwrap().subs.insert(remote::TOPIC_INDEX.into(), None);
        Ok(c)
    }

    /// Start the connection manager. Call once, inside a Tokio runtime.
    pub fn start(&self) {
        if self.0.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let rt = tokio::runtime::Handle::current();
        let _ = self.0.rt.set(rt.clone());
        rt.spawn(self.0.clone().manage());
    }

    pub fn shutdown(&self) {
        self.0.stop.cancel();
        if let Some(c) = self.0.st.lock().unwrap().conn.take() {
            c.kill.cancel();
        }
    }

    pub fn node_id(&self) -> String {
        self.0.saved.lock().unwrap().node_id.clone()
    }

    pub fn is_member(&self) -> bool {
        self.0.is_member()
    }

    /// Ask the member at `url` to admit this device. Progress shows in `status()`.
    pub fn join(&self, url: &str, passphrase: &str, name: &str) -> Result<()> {
        if passphrase.chars().count() < 8 {
            bail!("the mesh passphrase has at least 8 characters");
        }
        if self.is_member() {
            bail!("this device is already in a mesh; leave it first");
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            bail!("give this device a name of up to 64 characters");
        }
        self.0.saved.lock().unwrap().name = name.to_owned();
        self.0.save();
        self.0.st.lock().unwrap().joining = Some(Joining {
            url: normalize_url(url),
            passphrase: passphrase.to_owned(),
            code: None,
            status: "connecting".into(),
            error: None,
        });
        self.0.emit_status();
        self.0.kick.notify_one();
        Ok(())
    }

    pub fn cancel_join(&self) {
        let mut st = self.0.st.lock().unwrap();
        st.joining = None;
        if let Some(c) = st.conn.take().filter(|_| !self.0.is_member()) {
            c.kill.cancel();
        }
        drop(st);
        self.0.emit_status();
    }

    /// Forget the mesh. When a gateway is connected, tell it first with a signed removal of this
    /// device, so the members drop it from their lists; offline, they list it until one of them
    /// removes it.
    pub fn leave(&self) {
        self.0.announce_leaving();
        self.0.forget();
    }

    /// The network changed: check the connection now, and race for a gateway if it's gone.
    pub fn kick(&self) {
        let probe = self.0.st.lock().unwrap().conn.as_ref().map(|c| c.probe.clone());
        match probe {
            Some(p) => p.notify_one(),
            None => self.0.kick.notify_one(),
        }
    }

    /// In the foreground the client pings often; in the background, rarely.
    pub fn set_foreground(&self, fg: bool) {
        let was = {
            let mut st = self.0.st.lock().unwrap();
            let was = std::mem::replace(&mut st.foreground, fg);
            if !fg && was {
                st.hidden_since = Some(Instant::now());
            } else if fg && !was {
                let hidden_for = st.hidden_since.take().map(|t| t.elapsed()).unwrap_or_default();
                st.probe_timeout = Some(resume_probe_timeout(hidden_for));
            }
            was
        };
        if fg && !was {
            self.kick();
        }
    }

    pub fn status(&self) -> Value {
        self.0.status()
    }

    /// Admit a node waiting to join, with this device's own signature. The gateway passes the
    /// admission to the computer the node is connected to.
    pub async fn approve(&self, node_id: &str) -> Result<()> {
        let req = self.0.st.lock().unwrap().join_requests.get(node_id).cloned().ok_or_else(|| anyhow!("that request is gone"))?;
        let rec = {
            let s = self.0.saved.lock().unwrap();
            let mesh_id = s.mesh_id.clone().ok_or_else(|| anyhow!("this device isn't in a mesh"))?;
            Member::admit(&s.node_id, self.0.ident(), &mesh_id, node_id, &req.name, &req.sign_pub, &req.noise_pub, req.role.as_deref())
        };
        self.rpc("remote.admit", json!({ "record": rec })).await?;
        self.0.drop_request(node_id);
        Ok(())
    }

    /// Turn a node waiting to join away.
    pub async fn reject(&self, node_id: &str) -> Result<()> {
        self.rpc("remote.reject", json!({ "nodeId": node_id })).await?;
        self.0.drop_request(node_id);
        Ok(())
    }

    /// Call `method` on the gateway, waiting for one when none is connected.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.0.clone().rpc(method, params).await
    }

    /// Subscribe to a topic. `fresh` asks for a snapshot even if a point is known. The result
    /// includes events that arrived while the call was in flight.
    pub async fn subscribe(&self, topic: &str, fresh: bool) -> Result<Value> {
        let point = {
            let mut st = self.0.st.lock().unwrap();
            let p = st.subs.get(topic).cloned().flatten().filter(|_| !fresh);
            st.subs.insert(topic.to_owned(), p.clone());
            st.pending.insert(topic.to_owned(), Vec::new());
            p
        };
        let r = self.rpc("remote.subscribe", json!({ "topics": [{ "topic": topic, "point": point }] })).await;
        let r = match r {
            Ok(v) => v,
            Err(e) => {
                self.0.st.lock().unwrap().pending.remove(topic);
                return Err(e);
            }
        };
        let mut res = r["results"].get(0).cloned().ok_or_else(|| anyhow!("no subscribe result"))?;
        let pending = self.0.st.lock().unwrap().pending.remove(topic).unwrap_or_default();
        self.0.absorb(topic, &mut res, pending);
        Ok(res)
    }

    pub fn unsubscribe(&self, topic: &str) {
        if self.0.st.lock().unwrap().subs.remove(topic).is_none() {
            return;
        }
        let c = self.clone();
        let topic = topic.to_owned();
        // On the runtime `start` ran on: this may be called from a thread without one.
        if let Some(rt) = self.0.rt.get() {
            rt.spawn(async move {
                let _ = c.rpc("remote.unsubscribe", json!({ "topics": [topic] })).await;
            });
        }
    }

    /// Topics subscribed now.
    pub fn topics(&self) -> Vec<String> {
        self.0.st.lock().unwrap().subs.keys().cloned().collect()
    }
}

impl Inner {
    /// This device's identity (created on first start) and resume tickets, from the key store.
    fn load_keys(&self) -> Result<()> {
        if self.ident.get().is_some() {
            return Ok(());
        }
        let ident = match self.keys.get(IDENTITY) {
            Some(raw) => Identity::from_json(&raw).context("stored identity is corrupt")?,
            None => {
                let id = Identity::generate()?;
                if !self.keys.set(IDENTITY, &id.to_json()) {
                    bail!("could not store this device's keys");
                }
                id
            }
        };
        let tickets = self.keys.get(TICKETS).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        *self.tickets.lock().unwrap() = tickets;
        let _ = self.ident.set(ident);
        Ok(())
    }

    fn ident(&self) -> &Identity {
        self.ident.get().expect("keys load before any connection")
    }

    fn save(&self) {
        let s = serde_json::to_string_pretty(&*self.saved.lock().unwrap()).unwrap_or_default();
        if let Err(e) = write_private(&self.dir.join("remote.json"), s.as_bytes()) {
            tracing::warn!("remote: saving state: {e:#}");
        }
    }

    fn save_tickets(&self) {
        let s = serde_json::to_string(&*self.tickets.lock().unwrap()).unwrap_or_default();
        self.keys.set(TICKETS, &s);
    }

    fn is_member(&self) -> bool {
        let s = self.saved.lock().unwrap();
        s.mesh_id.is_some() && s.members.iter().any(|m| m.node_id == s.node_id && !m.is_removed())
    }

    fn emit(&self, name: &str, payload: Value) {
        (self.sink)(name, payload);
    }

    fn emit_status(&self) {
        self.emit(EV_STATUS, self.status());
    }

    fn status(&self) -> Value {
        let s = self.saved.lock().unwrap().clone();
        let st = self.st.lock().unwrap();
        let member = s.mesh_id.is_some() && s.members.iter().any(|m| m.node_id == s.node_id && !m.is_removed());
        let gw = st.conn.as_ref().map(|c| c.gateway.clone());
        let members: Vec<Value> = s
            .members
            .iter()
            .filter(|m| Some(&m.mesh_id) == s.mesh_id.as_ref() && !m.is_removed())
            .map(|m| {
                let state = if m.node_id == s.node_id {
                    "self".to_owned()
                } else if gw.as_deref() == Some(m.node_id.as_str()) {
                    "direct".to_owned()
                } else if gw.is_some() {
                    s.online.get(&m.node_id).cloned().unwrap_or_else(|| "offline".into())
                } else {
                    "offline".to_owned()
                };
                json!({
                    "nodeId": m.node_id,
                    "name": m.name,
                    "role": m.role,
                    "state": state,
                    "admittedAt": m.admitted_at,
                    "approver": m.approver,
                })
            })
            .collect();
        let joining = st.joining.as_ref().map(|j| json!({ "url": j.url, "code": j.code, "status": j.status, "error": j.error }));
        let host_name = |id: &str| s.members.iter().find(|m| m.node_id == id).map(|m| m.name.clone());
        let mut pending: Vec<Value> = st
            .join_requests
            .values()
            .map(|r| json!({ "nodeId": r.node_id, "name": r.name, "code": r.code, "role": r.role, "host": r.host, "hostName": host_name(&r.host) }))
            .collect();
        pending.sort_by_key(|p| p["nodeId"].as_str().map(str::to_owned));
        json!({
            "nodeId": s.node_id,
            "name": s.name,
            "member": member,
            "meshId": s.mesh_id,
            "phase": if member { st.phase.clone() } else { "offline".into() },
            "error": st.error,
            "gateway": gw,
            "gatewayName": st.conn.as_ref().map(|c| c.name.clone()),
            "gatewayUrl": st.conn.as_ref().map(|c| c.url.clone()),
            "rttMs": st.conn.as_ref().and_then(|c| c.rtt),
            "resumed": st.conn.as_ref().map(|c| c.resumed),
            "joining": joining,
            "members": members,
            "pending": pending,
            "running": true,
        })
    }

    /// Send the gateway this device's signed removal, then close the connection once it has had a
    /// moment to go out.
    fn announce_leaving(&self) {
        let (Some(ident), Some(rt)) = (self.ident.get(), self.rt.get()) else { return };
        let mut rec = {
            let s = self.saved.lock().unwrap();
            match s.members.iter().find(|m| m.node_id == s.node_id && !m.is_removed()) {
                Some(m) => m.clone(),
                None => return,
            }
        };
        let Some(c) = self.st.lock().unwrap().conn.take() else { return };
        rec.mark_removed(&rec.node_id.clone(), ident);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = c.tx.send(Msg::Rpc { id, method: "remote.leave".into(), params: json!({ "record": rec }) });
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            c.kill.cancel();
        });
    }

    fn forget(&self) {
        if let Some(c) = self.st.lock().unwrap().conn.take() {
            c.kill.cancel();
        }
        {
            let mut s = self.saved.lock().unwrap();
            let (id, name) = (s.node_id.clone(), s.name.clone());
            *s = Saved { node_id: id, name, ..Saved::default() };
        }
        self.save();
        self.tickets.lock().unwrap().clear();
        self.keys.delete(TICKETS);
        self.keys.delete(PASSPHRASE);
        self.st.lock().unwrap().joining = None;
        self.emit_status();
        self.emit(EV_MEMBERS, Value::Null);
    }

    fn set_phase(&self, phase: &str, error: Option<String>) {
        {
            let mut st = self.st.lock().unwrap();
            st.phase = phase.to_owned();
            st.error = error;
        }
        self.emit_status();
    }

    fn update_join(&self, f: impl FnOnce(&mut Joining)) {
        if let Some(j) = self.st.lock().unwrap().joining.as_mut() {
            f(j);
        }
        self.emit_status();
    }

    async fn rpc(self: Arc<Self>, method: &str, params: Value) -> Result<Value> {
        let tx = self.wait_conn().await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (wtx, wrx) = oneshot::channel();
        self.st.lock().unwrap().waiters.insert(id, wtx);
        tx.send(Msg::Rpc { id, method: method.to_owned(), params }).map_err(|_| anyhow!("the connection dropped"))?;
        match tokio::time::timeout(RPC_TIMEOUT, wrx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => Err(anyhow!("the connection dropped")),
            Err(_) => {
                self.st.lock().unwrap().waiters.remove(&id);
                Err(anyhow!("the computer didn't answer in time"))
            }
        }
    }

    async fn wait_conn(&self) -> Result<mpsc::UnboundedSender<Msg>> {
        let deadline = Instant::now() + CONNECT_WAIT;
        let mut rx = self.connected.subscribe();
        loop {
            if let Some(tx) = self.st.lock().unwrap().conn.as_ref().map(|c| c.tx.clone()) {
                return Ok(tx);
            }
            if !self.is_member() && self.st.lock().unwrap().joining.is_none() {
                bail!("this device isn't in a mesh");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, rx.changed()).await.is_err() {
                bail!("no computer in the mesh is reachable");
            }
        }
    }

    /// Fold events that arrived during a subscribe into its result.
    fn absorb(&self, topic: &str, res: &mut Value, pending: Vec<(String, Value)>) {
        let mut point: Option<Point> = serde_json::from_value(res["point"].clone()).ok();
        let key = if res["mode"] == "delta" { "events" } else { "tail" };
        let mut extra = Vec::new();
        for (name, payload) in pending {
            let p = remote::point_of(&payload);
            let newer = match (&p, &point) {
                (Some(p), Some(cur)) => p.s != cur.s || p.n > cur.n,
                _ => true,
            };
            if newer {
                if p.is_some() {
                    point = p;
                }
                extra.push(json!([name, payload]));
            }
        }
        if let Some(arr) = res[key].as_array_mut() {
            arr.extend(extra);
        } else if !extra.is_empty() {
            res[key] = Value::Array(extra);
        }
        res["point"] = json!(point);
        if let Some(sub) = self.st.lock().unwrap().subs.get_mut(topic) {
            *sub = point;
        }
    }

    // ---------------- connection manager ----------------

    async fn manage(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            if let Err(e) = self.load_keys() {
                tracing::warn!("remote: keys: {e:#}");
                self.set_phase("offline", Some(format!("{e:#}")));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let joining = self.st.lock().unwrap().joining.as_ref().filter(|j| j.status == "connecting").map(|j| j.url.clone());
            let est = if let Some(url) = joining {
                match self.clone().run_join(&url).await {
                    Ok(e) => Some(e),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        self.update_join(|j| {
                            if j.status == "connecting" || j.status == "waiting" {
                                j.status = "failed".into();
                            }
                            j.error = Some(msg);
                        });
                        None
                    }
                }
            } else if self.is_member() {
                let last_error = self.st.lock().unwrap().error.clone();
                self.set_phase("connecting", last_error);
                match self.clone().race().await {
                    Ok(e) => Some(e),
                    Err(e) => {
                        self.set_phase("offline", Some(format!("{e:#}")));
                        None
                    }
                }
            } else {
                None
            };
            match est {
                Some(e) => {
                    failures = 0;
                    self.clone().run_session(e).await;
                }
                None => {
                    failures += 1;
                    let wait = if self.is_member() {
                        Duration::from_secs(2u64.pow(failures.min(5)).min(30))
                    } else {
                        Duration::from_secs(3600)
                    };
                    tokio::select! {
                        _ = self.stop.cancelled() => return,
                        _ = self.kick.notified() => failures = 0,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
    }

    fn hello(&self, joining: bool) -> Hello {
        let s = self.saved.lock().unwrap();
        let record = s.members.iter().find(|m| m.node_id == s.node_id && !m.is_removed()).cloned();
        Hello {
            node_id: s.node_id.clone(),
            name: s.name.clone(),
            sign_pub: self.ident().sign_pub_hex(),
            version: PROTOCOL_VERSION,
            mesh_id: if joining { None } else { s.mesh_id.clone() },
            record: if joining { None } else { record },
            joining,
            data_key_gen: 0,
            addrs: Vec::new(),
            listen_port: None,
            role: Some(ROLE_REMOTE.into()),
            features: Vec::new(),
        }
    }

    /// The gateway speaks first; only a node that serves remote members gets our Hello, so an
    /// older node never mistakes this device for a full member.
    async fn their_hello(sec: &mut Secure, link: &mut Link) -> Result<Hello> {
        let h = match tokio::time::timeout(HELLO_TIMEOUT, recv(sec, link)).await {
            Ok(Ok(Some(Msg::Hello(h)))) => *h,
            Ok(Err(e)) => return Err(e),
            _ => bail!("no hello"),
        };
        if !h.features.iter().any(|f| f == FEATURE_GATEWAY) {
            bail!("{} runs a Patchbay too old to serve a phone; update it", h.name);
        }
        if h.role.as_deref() == Some(ROLE_REMOTE) {
            bail!("{} is a phone, not a computer", h.name);
        }
        Ok(h)
    }

    async fn run_join(self: Arc<Self>, url: &str) -> Result<Established> {
        let pass = self.st.lock().unwrap().joining.as_ref().map(|j| j.passphrase.clone()).unwrap_or_default();
        let mut link = ws::dial(url, DIAL_TIMEOUT).await?;
        let mut sec = match handshake::initiate(&mut link, &pass, &self.ident().noise_private).await {
            Ok(s) => s,
            Err(HandshakeError::WrongPassphrase) => bail!("The passphrase doesn't match this mesh's"),
            Err(e) => return Err(e.into()),
        };
        let theirs = Self::their_hello(&mut sec, &mut link).await?;
        let Some(mesh_id) = theirs.mesh_id.clone() else { bail!("{} isn't in a mesh", theirs.name) };
        send(&mut sec, &link, &Msg::Hello(Box::new(self.hello(true))))?;
        let code = handshake::confirmation_code(&self.ident().noise_public, &sec.remote_static);
        self.update_join(|j| {
            j.code = Some(code.clone());
            j.status = "waiting".into();
            j.error = None;
        });
        loop {
            let m = tokio::select! {
                _ = self.stop.cancelled() => bail!("stopped"),
                m = recv(&mut sec, &mut link) => m?,
            };
            if self.st.lock().unwrap().joining.is_none() {
                bail!("cancelled");
            }
            match m {
                None => bail!("{} closed the connection", theirs.name),
                Some(Msg::JoinPending { .. }) => {}
                Some(Msg::Rejected { reason }) => {
                    self.update_join(|j| j.status = "rejected".into());
                    bail!("{reason}");
                }
                Some(Msg::Admitted { members, .. }) => {
                    let me = self.saved.lock().unwrap().node_id.clone();
                    let mine = members.iter().find(|m| m.node_id == me);
                    let ok = mine.is_some_and(|m| {
                        m.noise_pub == self.ident().noise_pub_hex()
                            && m.sign_pub == self.ident().sign_pub_hex()
                            && m.is_remote()
                            && m.mesh_id == mesh_id
                    });
                    if !ok {
                        bail!("the admission record doesn't match this device");
                    }
                    {
                        let mut s = self.saved.lock().unwrap();
                        s.mesh_id = Some(mesh_id.clone());
                        s.members = members.into_iter().filter(|m| m.mesh_id == mesh_id).collect();
                        s.addrs.entry(theirs.node_id.clone()).or_default().insert(url.to_owned());
                        s.last_gateway = Some(theirs.node_id.clone());
                    }
                    self.save();
                    self.keys.set(PASSPHRASE, &pass);
                    self.st.lock().unwrap().joining = None;
                    self.emit(EV_MEMBERS, Value::Null);
                    return Ok(Established {
                        sec,
                        link,
                        gateway: theirs.node_id,
                        name: theirs.name,
                        url: url.to_owned(),
                        resumed: false,
                    });
                }
                Some(_) => {}
            }
        }
    }

    /// Dial every known address of every full member, staggered in order of preference, and keep
    /// the first connection that completes its handshake.
    async fn race(self: Arc<Self>) -> Result<Established> {
        let cands = self.candidates();
        if cands.is_empty() {
            bail!("no known address for any computer in the mesh");
        }
        let won = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        for (i, (node, url)) in cands.into_iter().enumerate() {
            let (inner, won, tx) = (self.clone(), won.clone(), tx.clone());
            tasks.spawn(async move {
                tokio::select! {
                    _ = won.cancelled() => return,
                    _ = tokio::time::sleep(STAGGER * i as u32) => {}
                }
                let r = tokio::select! {
                    _ = won.cancelled() => return,
                    r = inner.attempt(&node, &url) => r,
                };
                let _ = tx.send(r.map_err(|e| format!("{url}: {e:#}")));
            });
        }
        drop(tx);
        let mut errors = Vec::new();
        let result = loop {
            tokio::select! {
                _ = self.stop.cancelled() => break Err(anyhow!("stopped")),
                r = rx.recv() => match r {
                    Some(Ok(e)) => break Ok(e),
                    Some(Err(e)) => errors.push(e),
                    None => break Err(anyhow!("no computer in the mesh answered ({})", errors.join("; "))),
                },
            }
        };
        won.cancel();
        tasks.abort_all();
        result
    }

    fn candidates(&self) -> Vec<(String, String)> {
        let s = self.saved.lock().unwrap();
        let mut nodes: Vec<&Member> = s
            .members
            .iter()
            .filter(|m| m.node_id != s.node_id && !m.is_removed() && !m.is_remote())
            .collect();
        nodes.sort_by_key(|m| {
            let last = s.last_gateway.as_deref() == Some(m.node_id.as_str());
            let online = s.online.get(&m.node_id).is_some_and(|x| x != "offline");
            (!last, !online, s.rtt.get(&m.node_id).copied().unwrap_or(u32::MAX))
        });
        let mut out = Vec::new();
        // One address per node first, then the rest, so every node gets an early try.
        let per: Vec<Vec<String>> = nodes
            .iter()
            .map(|m| s.addrs.get(&m.node_id).map(|a| a.iter().cloned().collect()).unwrap_or_default())
            .collect();
        let depth = per.iter().map(Vec::len).max().unwrap_or(0);
        for d in 0..depth {
            for (m, urls) in nodes.iter().zip(&per) {
                if let Some(u) = urls.get(d) {
                    out.push((m.node_id.clone(), u.clone()));
                }
            }
        }
        out.truncate(MAX_CANDIDATES);
        out
    }

    async fn attempt(&self, node: &str, url: &str) -> Result<Established> {
        let member = self
            .saved
            .lock()
            .unwrap()
            .members
            .iter()
            .find(|m| m.node_id == node)
            .cloned()
            .ok_or_else(|| anyhow!("unknown member"))?;
        let gw_static = hex32(&member.noise_pub)?;
        let now = member::now_ms();
        let ticket = self.tickets.lock().unwrap().get(node).filter(|t| t.expires > now).cloned();
        let mut resumed = false;
        let mut link = ws::dial(url, DIAL_TIMEOUT).await?;
        let mut sec = None;
        if let Some(t) = ticket {
            let id: [u8; TICKET_ID_LEN] = hex::decode(&t.id)?.try_into().map_err(|_| anyhow!("bad ticket"))?;
            match handshake::initiate_resume(&mut link, &self.ident().noise_private, &gw_static, &id, &hex32(&t.key)?).await {
                Ok(s) => {
                    sec = Some(s);
                    resumed = true;
                }
                Err(HandshakeError::UnknownTicket) => {
                    self.tickets.lock().unwrap().remove(node);
                    self.save_tickets();
                    link = ws::dial(url, DIAL_TIMEOUT).await?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        let mut sec = match sec {
            Some(s) => s,
            None => {
                let pass = self.keys.get(PASSPHRASE).ok_or_else(|| anyhow!("the mesh passphrase is missing"))?;
                match handshake::initiate(&mut link, &pass, &self.ident().noise_private).await {
                    Ok(s) => s,
                    Err(HandshakeError::WrongPassphrase) => bail!("the mesh passphrase changed; join again"),
                    Err(e) => return Err(e.into()),
                }
            }
        };
        if sec.remote_static != gw_static {
            bail!("{url} answered with keys that aren't {}'s", member.name);
        }
        let theirs = Self::their_hello(&mut sec, &mut link).await?;
        if theirs.node_id != node {
            bail!("{url} is now another node");
        }
        send(&mut sec, &link, &Msg::Hello(Box::new(self.hello(false))))?;
        Ok(Established { sec, link, gateway: node.to_owned(), name: theirs.name, url: url.to_owned(), resumed })
    }

    async fn run_session(self: Arc<Self>, e: Established) {
        let Established { mut sec, mut link, gateway, name, url, resumed } = e;
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let kill = self.stop.child_token();
        let probe = Arc::new(Notify::new());
        {
            let mut st = self.st.lock().unwrap();
            st.conn = Some(Conn {
                gateway: gateway.clone(),
                name: name.clone(),
                url: url.clone(),
                tx: tx.clone(),
                kill: kill.clone(),
                probe: probe.clone(),
                rtt: None,
                resumed,
            });
            st.phase = "online".into();
            st.error = None;
        }
        {
            let mut s = self.saved.lock().unwrap();
            s.last_gateway = Some(gateway.clone());
            s.addrs.entry(gateway.clone()).or_default().insert(url.clone());
        }
        self.save();
        tracing::info!("remote: connected to {name} at {url}{}", if resumed { " (resumed)" } else { "" });
        self.connected.send_replace(true);
        self.emit_status();
        tokio::spawn(self.clone().after_connect());

        let mut last_seen = Instant::now();
        let mut probe_until: Option<Instant> = None;
        let interval = |fg: bool| Duration::from_secs(if fg { 15 } else { 60 });
        let mut ping = tokio::time::interval(interval(true));
        let mut fg = true;
        let res: Result<()> = async {
            loop {
                let now_fg = self.st.lock().unwrap().foreground;
                if now_fg != fg {
                    fg = now_fg;
                    ping = tokio::time::interval(interval(fg));
                }
                let probe_wait = async {
                    match probe_until {
                        Some(t) => tokio::time::sleep_until(t.into()).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = kill.cancelled() => return Ok(()),
                    m = recv(&mut sec, &mut link) => {
                        let Some(m) = m? else { bail!("{name} closed the connection") };
                        last_seen = Instant::now();
                        probe_until = None;
                        match m {
                            Msg::Event { name: ev, payload } => self.on_event(&ev, payload),
                            Msg::RpcResult { id, ok, err } => {
                                if let Some(w) = self.st.lock().unwrap().waiters.remove(&id) {
                                    let _ = w.send(match err { Some(e) => Err(e), None => Ok(ok.unwrap_or(Value::Null)) });
                                }
                            }
                            Msg::Rpc { id, .. } => send(&mut sec, &link, &Msg::RpcResult { id, ok: None, err: Some("a phone serves no requests".into()) })?,
                            Msg::Ticket { id, key, expires } => {
                                self.tickets.lock().unwrap().insert(gateway.clone(), Ticket { id, key, expires });
                                self.save_tickets();
                            }
                            Msg::Ping { t } => send(&mut sec, &link, &Msg::Pong { t })?,
                            Msg::Pong { t } => {
                                let rtt = (member::now_ms() - t).clamp(0, u32::MAX as i64) as u32;
                                if let Some(c) = self.st.lock().unwrap().conn.as_mut() {
                                    c.rtt = Some(rtt);
                                }
                                self.saved.lock().unwrap().rtt.insert(gateway.clone(), rtt);
                            }
                            Msg::Rejected { reason } => {
                                tracing::warn!("remote: {name} refused this device: {reason}");
                                if reason.contains("removed") {
                                    self.forget();
                                }
                                bail!("{reason}");
                            }
                            _ => {}
                        }
                    }
                    out = rx.recv() => {
                        let Some(out) = out else { return Ok(()) };
                        send(&mut sec, &link, &out)?;
                    }
                    _ = ping.tick() => {
                        if last_seen.elapsed() > interval(fg) * 3 {
                            bail!("{name} stopped responding");
                        }
                        send(&mut sec, &link, &Msg::Ping { t: member::now_ms() })?;
                    }
                    _ = probe.notified() => {
                        send(&mut sec, &link, &Msg::Ping { t: member::now_ms() })?;
                        let wait = self.st.lock().unwrap().probe_timeout.take().unwrap_or(PROBE_TIMEOUT);
                        let until = Instant::now() + wait;
                        // A second nudge while one probe runs never extends it.
                        probe_until = Some(probe_until.map_or(until, |u| u.min(until)));
                    }
                    _ = probe_wait => bail!("{name} didn't answer after the network changed"),
                }
            }
        }
        .await;
        let waiters: Vec<_> = {
            let mut st = self.st.lock().unwrap();
            if st.conn.as_ref().is_some_and(|c| c.gateway == gateway) {
                st.conn = None;
            }
            st.phase = "connecting".into();
            st.error = res.as_ref().err().map(|e| format!("{e:#}"));
            // The next gateway lists what waits when we connect to it.
            st.join_requests.clear();
            st.waiters.drain().map(|(_, w)| w).collect()
        };
        for w in waiters {
            let _ = w.send(Err("the connection dropped".into()));
        }
        self.connected.send_replace(false);
        self.save();
        if let Err(e) = &res {
            tracing::info!("remote: disconnected from {name}: {e:#}");
        }
        self.emit_status();
    }

    /// A node waits to join. Believed when its host is a member we know and its code is the one
    /// that host's key and the node's key give, so the code shown is the one we'd sign for.
    fn join_requested(&self, payload: Value) {
        let Ok(req) = serde_json::from_value::<JoinRequest>(payload.clone()) else { return };
        let me = self.saved.lock().unwrap().node_id.clone();
        let host_ok = self
            .saved
            .lock()
            .unwrap()
            .members
            .iter()
            .find(|m| m.node_id == req.host)
            .is_some_and(|h| !h.is_removed() && !h.is_remote() && req.code_matches(&h.noise_pub));
        if !host_ok || req.node_id == me || !self.is_member() {
            tracing::debug!("remote: ignoring a join request that doesn't add up");
            return;
        }
        let fresh = self.st.lock().unwrap().join_requests.insert(req.node_id.clone(), req).is_none();
        if fresh {
            self.emit_status();
            self.emit(EV_JOIN_REQUEST, payload);
        }
    }

    /// A join request is decided or gone.
    fn drop_request(&self, node_id: &str) {
        if self.st.lock().unwrap().join_requests.remove(node_id).is_some() {
            self.emit_status();
            self.emit(EV_JOIN_RESOLVED, json!({ "nodeId": node_id }));
        }
    }

    fn on_event(&self, name: &str, payload: Value) {
        if name == EV_JOIN_REQUEST {
            self.join_requested(payload);
            return;
        }
        if name == EV_JOIN_RESOLVED {
            if let Some(id) = payload["nodeId"].as_str() {
                self.drop_request(id);
            }
            return;
        }
        if let Some(topic) = remote::topic_of(name, &payload) {
            let mut st = self.st.lock().unwrap();
            if let Some(buf) = st.pending.get_mut(&topic) {
                buf.push((name.to_owned(), payload));
                return;
            }
            if let (Some(sub), Some(p)) = (st.subs.get_mut(&topic), remote::point_of(&payload)) {
                *sub = Some(p);
            }
        }
        self.emit(name, payload);
    }

    /// Resubscribe from the last points, then refresh the member list.
    async fn after_connect(self: Arc<Self>) {
        let topics: Vec<(String, Option<Point>)> = {
            let mut st = self.st.lock().unwrap();
            let t: Vec<_> = st.subs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, _) in &t {
                st.pending.insert(k.clone(), Vec::new());
            }
            t
        };
        let req: Vec<Value> = topics.iter().map(|(t, p)| json!({ "topic": t, "point": p })).collect();
        match self.clone().rpc("remote.subscribe", json!({ "topics": req })).await {
            Ok(r) => {
                for mut res in r["results"].as_array().cloned().unwrap_or_default() {
                    let topic = res["topic"].as_str().unwrap_or_default().to_owned();
                    let pending = self.st.lock().unwrap().pending.remove(&topic).unwrap_or_default();
                    self.absorb(&topic, &mut res, pending);
                    if res["mode"] == "delta" {
                        for e in res["events"].as_array().cloned().unwrap_or_default() {
                            if let (Some(n), Some(p)) = (e[0].as_str(), e.get(1)) {
                                self.emit(n, p.clone());
                            }
                        }
                    } else {
                        self.emit(EV_RESYNC, res);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("remote: resubscribe: {e:#}");
                self.st.lock().unwrap().pending.clear();
            }
        }
        if let Err(e) = self.refresh_members().await {
            tracing::debug!("remote: member list: {e:#}");
        }
    }

    async fn refresh_members(self: Arc<Self>) -> Result<()> {
        let r = self.clone().rpc("remote.members", Value::Null).await?;
        let recs: Vec<Member> = serde_json::from_value(r["members"].clone()).unwrap_or_default();
        let addrs: BTreeMap<String, BTreeSet<String>> = serde_json::from_value(r["addrs"].clone()).unwrap_or_default();
        let online: BTreeMap<String, String> = serde_json::from_value(r["online"].clone()).unwrap_or_default();
        let removed_me = {
            let mut s = self.saved.lock().unwrap();
            let mesh = s.mesh_id.clone();
            // Accept records the ones we trust vouch for, repeating as chains of approvals resolve.
            let mut known = s.members.clone();
            let mut todo: Vec<Member> = recs.into_iter().filter(|m| Some(&m.mesh_id) == mesh.as_ref()).collect();
            loop {
                let before = todo.len();
                todo.retain(|m| match member::verify_against(&known, m) {
                    Ok(true) => {
                        known.retain(|k| k.node_id != m.node_id);
                        known.push(m.clone());
                        false
                    }
                    Ok(false) => true,
                    Err(e) => {
                        tracing::warn!("remote: {e:#}");
                        false
                    }
                });
                if todo.len() == before {
                    break;
                }
            }
            s.members = known;
            for (n, urls) in addrs {
                s.addrs.entry(n).or_default().extend(urls.iter().map(|u| normalize_url(u)));
            }
            s.online = online;
            let me = s.node_id.clone();
            s.members.iter().any(|m| m.node_id == me && m.is_removed())
        };
        if removed_me {
            tracing::warn!("remote: this device was removed from the mesh");
            self.forget();
            return Ok(());
        }
        self.save();
        self.emit(EV_MEMBERS, Value::Null);
        self.emit_status();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coming_back_after_a_while_probes_briefly() {
        // A quick glance away keeps the patient probe: the connection is likely still there.
        assert_eq!(resume_probe_timeout(Duration::from_secs(5)), PROBE_TIMEOUT);
        // Long enough for Android to have frozen the app: dial again sooner.
        assert_eq!(resume_probe_timeout(STALE_AFTER), RESUME_PROBE_TIMEOUT);
        assert_eq!(resume_probe_timeout(Duration::from_secs(600)), RESUME_PROBE_TIMEOUT);
        assert!(RESUME_PROBE_TIMEOUT < PROBE_TIMEOUT);
    }
}
