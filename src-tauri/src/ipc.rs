//! Local IPC between patchbayd and a desktop app started on the same data folder.
//!
//! Only one process may own a data folder (`instance.lock`, see `open_state`). When patchbayd owns
//! it, a desktop app started afterwards can't open the database itself. Instead it attaches to the
//! daemon over a socket in the folder (a named pipe on Windows) and uses it as its backend: the same
//! commands, channels and app events the browser UI gets over its WebSocket (see web.rs).
//!
//! Wire format: one JSON object per line. The handshake comes first:
//! 1. client: `{"hello": nonce_c}`
//! 2. server: `{"nonce": nonce_s, "proof": proof(token, "server", nonce_c)}`
//! 3. client, after checking the proof: `{"auth": proof(token, "client", nonce_s)}`
//! 4. server: `{"ready": true, "version": v}`, or `{"authError": message}` and it hangs up.
//!
//! `token` is the `ipc-token` file in the data folder: new on every daemon start and readable by
//! this user only. Both sides prove they know it without sending it, so a process that takes the
//! socket's name first (possible with named pipes) learns nothing and can't pose as the daemon.
//!
//! Then the frames of web.rs: `{"id", "cmd", "args"}` from the client, and `{"id", "ok"}`,
//! `{"id", "err"}`, `{"chan", "msg"}`, `{"chanEnd"}` and `{"event", "payload"}` from the server.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The token file in the data folder.
const TOKEN_FILE: &str = "ipc-token";
/// Longest handshake line either side accepts.
const HANDSHAKE_MAX: u64 = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// This build's version. The app attaches only to a daemon of the same version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon's socket in `data_dir`.
#[cfg(unix)]
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("patchbayd.sock")
}

/// The daemon's named pipe for `data_dir`. Pipe names are global, so the name comes from the
/// folder's path, canonical so that the app and the daemon agree however each one spells it.
#[cfg(windows)]
pub fn pipe_name(data_dir: &Path) -> String {
    let dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let key = dir.to_string_lossy().to_lowercase();
    format!(r"\\.\pipe\patchbay-{}", hex::encode(&Sha256::digest(key.as_bytes())[..12]))
}

/// What one side sends to show it knows `token`, for the other side's `nonce`.
fn proof(token: &str, role: &str, nonce: &str) -> String {
    let mut h = Sha256::new();
    for part in ["patchbay-ipc", role, token, nonce] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

fn nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Compare secrets in constant time.
pub(crate) fn token_matches(given: &str, want: &str) -> bool {
    given.len() == want.len() && given.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Read one handshake line, refusing anything longer than `HANDSHAKE_MAX`.
async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Value> {
    let mut line = String::new();
    if (&mut *r).take(HANDSHAKE_MAX).read_line(&mut line).await? == 0 {
        bail!("the connection closed");
    }
    if !line.ends_with('\n') {
        bail!("handshake line too long");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    w.write_all(format!("{v}\n").as_bytes()).await?;
    Ok(())
}

#[cfg(any(feature = "desktop", test))]
fn read_token(data_dir: &Path) -> Result<String> {
    let t = std::fs::read_to_string(data_dir.join(TOKEN_FILE))?;
    let t = t.trim();
    if t.is_empty() {
        bail!("{TOKEN_FILE} is empty");
    }
    Ok(t.to_owned())
}

/// The daemon's side: the socket server and the command loop it shares with web.rs.
#[cfg(feature = "daemon")]
pub mod server {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use futures::{Stream, StreamExt};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio::sync::{broadcast, mpsc};

    use super::*;
    use crate::AppState;
    use crate::api::{self, ChanFactory, ChanSend};

    /// App events for every attached client, already serialized.
    pub type Events = broadcast::Sender<Arc<str>>;

    /// A `UiSink` that hands events to the attached clients.
    pub fn ui_sink(events: Events) -> crate::mesh::node::UiSink {
        Arc::new(move |name: &str, payload: Value| {
            // No receivers just means no client is attached.
            let _ = events.send(json!({ "event": name, "payload": payload }).to_string().into());
        })
    }

    /// The frame a client gets once it is signed in.
    pub fn ready_frame() -> Arc<str> {
        json!({ "ready": true, "version": VERSION }).to_string().into()
    }

    /// Sends `{"chanEnd": n}` when the last handle to channel `n` drops, after its last message,
    /// so the client can forget the channel.
    struct ChanEnd {
        n: u64,
        out: mpsc::UnboundedSender<Arc<str>>,
    }

    impl Drop for ChanEnd {
        fn drop(&mut self) {
            let _ = self.out.send(json!({ "chanEnd": self.n }).to_string().into());
        }
    }

    /// Serve one signed-in client until `incoming` ends: run each `{"id", "cmd", "args"}` frame
    /// through `api::call`, and queue the replies, channel messages and app events on `out`.
    /// `events` should be subscribed before the client is told it is ready, so it misses nothing.
    pub async fn serve_session<S>(
        state: AppState,
        mut events: broadcast::Receiver<Arc<str>>,
        mut incoming: S,
        out: mpsc::UnboundedSender<Arc<str>>,
    ) where
        S: Stream<Item = String> + Unpin,
    {
        let ev_out = out.clone();
        let forward = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(e) => {
                        if ev_out.send(e).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("a client fell behind; dropped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let chans: ChanFactory = {
            let out = out.clone();
            Arc::new(move |id: &str| {
                let n: u64 = id.strip_prefix("__CHANNEL__:")?.parse().ok()?;
                let end = ChanEnd { n, out: out.clone() };
                Some(Arc::new(move |msg: Value| {
                    end.out.send(json!({ "chan": end.n, "msg": msg }).to_string().into()).is_ok()
                }) as ChanSend)
            })
        };

        while let Some(text) = incoming.next().await {
            let Ok(mut req) = serde_json::from_str::<Value>(&text) else { continue };
            let id = req["id"].clone();
            let cmd = req["cmd"].as_str().unwrap_or_default().to_owned();
            let args = req["args"].take();
            let (state, chans, out) = (state.clone(), chans.clone(), out.clone());
            tokio::spawn(async move {
                let reply = match api::call(&state, &cmd, args, &chans).await {
                    Ok(v) => json!({ "id": id, "ok": v }),
                    Err(e) => json!({ "id": id, "err": e.0 }),
                };
                let _ = out.send(reply.to_string().into());
            });
        }
        forward.abort();
    }

    /// Write a new token and start serving the socket for `data_dir`. Call it with the folder's
    /// lock held (after `open_state`): the lock makes any socket already there a stale one.
    pub fn start(state: AppState, events: Events, data_dir: &Path) -> Result<()> {
        let token: Arc<str> = write_token(data_dir)?.into();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = socket_path(data_dir);
            let _ = std::fs::remove_file(&path);
            let listener =
                tokio::net::UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(handle(state.clone(), events.clone(), token.clone(), stream));
                        }
                        Err(e) => {
                            tracing::warn!("local socket: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            });
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = pipe_name(data_dir);
            // `first_pipe_instance` fails if another process already holds the name.
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .with_context(|| format!("creating pipe {name}"))?;
            tokio::spawn(async move {
                let mut server = first;
                loop {
                    let connected = server.connect().await;
                    // A new instance before handing this one over, so the next client finds one.
                    let next = loop {
                        match ServerOptions::new().reject_remote_clients(true).create(&name) {
                            Ok(n) => break n,
                            Err(e) => {
                                tracing::warn!("pipe {name}: {e}; trying again");
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            }
                        }
                    };
                    let this = std::mem::replace(&mut server, next);
                    match connected {
                        Ok(()) => {
                            tokio::spawn(handle(state.clone(), events.clone(), token.clone(), this));
                        }
                        Err(e) => tracing::warn!("pipe {name}: {e}"),
                    }
                }
            });
        }

        Ok(())
    }

    /// Remove the socket and token at shutdown.
    pub fn cleanup(data_dir: &Path) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(socket_path(data_dir));
        let _ = std::fs::remove_file(data_dir.join(TOKEN_FILE));
    }

    fn write_token(data_dir: &Path) -> Result<String> {
        let token = hex::encode(rand::random::<[u8; 32]>());
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let path = data_dir.join(TOKEN_FILE);
        std::io::Write::write_all(&mut opts.open(&path)?, token.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(token)
    }

    async fn handshake<R, W>(r: &mut R, w: &mut W, token: &str) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let m = read_frame(r).await?;
        let theirs = m["hello"].as_str().filter(|n| n.len() <= 128).context("expected hello")?.to_owned();
        let mine = nonce();
        write_frame(w, &json!({ "nonce": mine, "proof": proof(token, "server", &theirs) })).await?;
        let m = read_frame(r).await?;
        if !m["auth"].as_str().is_some_and(|a| token_matches(a, &proof(token, "client", &mine))) {
            anyhow::bail!("wrong token");
        }
        Ok(())
    }

    async fn handle<T>(state: AppState, events: Events, token: Arc<str>, io: T)
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, mut w) = tokio::io::split(io);
        let mut r = BufReader::new(r);
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut r, &mut w, &token)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = write_frame(&mut w, &json!({ "authError": e.to_string() })).await;
                return;
            }
            Err(_) => return,
        }

        let rx_events = events.subscribe();
        let (tx, mut rx) = mpsc::unbounded_channel::<Arc<str>>();
        let _ = tx.send(ready_frame());
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if w.write_all(format!("{m}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let lines = futures::stream::unfold(r.lines(), |mut l| async move {
            l.next_line().await.ok().flatten().map(|s| (s, l))
        });
        serve_session(state, rx_events, std::pin::pin!(lines), tx).await;
        writer.abort();
    }
}

/// The desktop app's side: a connection to the daemon that runs commands on it.
#[cfg(any(feature = "desktop", test))]
pub mod client {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::api::{ChanFactory, ChanSend};

    /// What `Conn::call` answers once the connection is gone.
    pub const LOST: &str = "lost the connection to patchbayd";

    /// Command arguments that are frontend channels (`api::Chan`).
    const CHANNEL_ARGS: &[&str] = &["onEvent"];

    /// Receives the daemon's app events: name and payload.
    pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

    #[derive(Debug, thiserror::Error)]
    pub enum AttachError {
        /// No daemon listens on the folder (or none yet).
        #[error("no patchbayd answers on this data folder: {0}")]
        Unreachable(std::io::Error),
        #[error(
            "patchbayd {daemon} is running on this data folder, and this app is {app}. Install the same \
             version of both, or stop patchbayd to use the app on its own."
        )]
        Version { daemon: String, app: String },
        #[error("{0:#}")]
        Other(anyhow::Error),
    }

    type Reply = Result<Value, String>;

    /// One signed-in connection to the daemon.
    pub struct Conn {
        out: mpsc::UnboundedSender<String>,
        pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
        /// Channels passed to the daemon, by the number it knows them by.
        chans: Mutex<HashMap<u64, ChanSend>>,
        next: AtomicU64,
        closed: AtomicBool,
    }

    impl Conn {
        /// Connect to the daemon on `data_dir` and sign in. `on_event` gets its app events, and
        /// `on_close` runs once when the connection ends. Must run inside the async runtime.
        pub async fn open(
            data_dir: &Path,
            on_event: EventSink,
            on_close: impl FnOnce() + Send + 'static,
        ) -> Result<Arc<Conn>, AttachError> {
            #[cfg(unix)]
            let io = tokio::net::UnixStream::connect(socket_path(data_dir)).await.map_err(AttachError::Unreachable)?;
            #[cfg(windows)]
            let io = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(pipe_name(data_dir))
                .map_err(AttachError::Unreachable)?;

            let token = read_token(data_dir).map_err(AttachError::Other)?;
            let (r, mut w) = tokio::io::split(io);
            let mut r = BufReader::new(r);
            let version = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut r, &mut w, &token))
                .await
                .map_err(|_| AttachError::Other(anyhow::anyhow!("patchbayd didn't answer")))?
                .map_err(AttachError::Other)?;
            if version != VERSION {
                return Err(AttachError::Version { daemon: version, app: VERSION.to_owned() });
            }

            let (out, mut rx) = mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if w.write_all(format!("{m}\n").as_bytes()).await.is_err() {
                        break;
                    }
                }
            });

            let conn = Arc::new(Conn {
                out,
                pending: Mutex::default(),
                chans: Mutex::default(),
                next: AtomicU64::new(1),
                closed: AtomicBool::new(false),
            });
            let reader = conn.clone();
            tokio::spawn(async move {
                let mut lines = r.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(m) = serde_json::from_str::<Value>(&line) {
                        reader.receive(m, &on_event);
                    }
                }
                reader.closed.store(true, Ordering::SeqCst);
                for (_, tx) in reader.pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                    let _ = tx.send(Err(LOST.to_owned()));
                }
                reader.chans.lock().unwrap_or_else(|e| e.into_inner()).clear();
                on_close();
            });
            Ok(conn)
        }

        fn receive(&self, mut m: Value, on_event: &EventSink) {
            if let Some(id) = m.get("id").and_then(Value::as_u64) {
                let tx = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                if let Some(tx) = tx {
                    let reply = match m.get_mut("err").map(Value::take) {
                        Some(Value::String(e)) => Err(e),
                        Some(e) => Err(e.to_string()),
                        None => Ok(m.get_mut("ok").map(Value::take).unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(reply);
                }
            } else if let Some(n) = m.get("chan").and_then(Value::as_u64) {
                let send = self.chans.lock().unwrap_or_else(|e| e.into_inner()).get(&n).cloned();
                if let Some(send) = send
                    && !send(m.get_mut("msg").map(Value::take).unwrap_or(Value::Null))
                {
                    self.chans.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
                }
            } else if let Some(n) = m.get("chanEnd").and_then(Value::as_u64) {
                // The daemon dropped the channel: no more messages will come for it.
                self.chans.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
            } else if let Some(name) = m.get("event").and_then(Value::as_str).map(str::to_owned) {
                on_event(&name, m.get_mut("payload").map(Value::take).unwrap_or(Value::Null));
            }
        }

        /// Channels the daemon may still send on.
        #[cfg(test)]
        fn open_channels(&self) -> usize {
            self.chans.lock().unwrap().len()
        }

        /// Run `cmd` on the daemon. Channel arguments are opened through `chans`, as `api::call`
        /// would, and the daemon's messages for them are passed on.
        pub async fn call(&self, cmd: &str, mut args: Value, chans: &ChanFactory) -> Result<Value, String> {
            for key in CHANNEL_ARGS {
                let Some(id) = args.get(*key).and_then(Value::as_str).map(str::to_owned) else { continue };
                let send = chans(&id).ok_or_else(|| format!("invalid argument {key}: invalid channel"))?;
                let n = self.next.fetch_add(1, Ordering::Relaxed);
                self.chans.lock().unwrap_or_else(|e| e.into_inner()).insert(n, send);
                args[*key] = Value::String(format!("__CHANNEL__:{n}"));
            }
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
            // The reader drains `pending` after setting `closed`: a call that lands after that
            // must not wait for a reply that never comes.
            if self.closed.load(Ordering::SeqCst)
                || self.out.send(json!({ "id": id, "cmd": cmd, "args": args }).to_string()).is_err()
            {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                return Err(LOST.to_owned());
            }
            rx.await.unwrap_or_else(|_| Err(LOST.to_owned()))
        }
    }

    /// Sign in; returns the daemon's version.
    async fn handshake<R, W>(r: &mut R, w: &mut W, token: &str) -> anyhow::Result<String>
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mine = nonce();
        write_frame(w, &json!({ "hello": mine })).await?;
        let m = read_frame(r).await?;
        let (Some(theirs), Some(p)) = (m["nonce"].as_str(), m["proof"].as_str()) else {
            anyhow::bail!("patchbayd sent no proof");
        };
        if theirs.len() > 128 || !token_matches(p, &proof(token, "server", &mine)) {
            anyhow::bail!("the process on the socket isn't this data folder's patchbayd");
        }
        write_frame(w, &json!({ "auth": proof(token, "client", theirs) })).await?;
        let m = read_frame(r).await?;
        if m["ready"] != true {
            anyhow::bail!("patchbayd refused the app: {}", m["authError"].as_str().unwrap_or("no reason given"));
        }
        Ok(m["version"].as_str().unwrap_or_default().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"));
    }

    #[test]
    fn proofs_differ_by_role_token_and_nonce() {
        let p = proof("t", "server", "n");
        assert_eq!(p, proof("t", "server", "n"));
        assert_ne!(p, proof("t", "client", "n"));
        assert_ne!(p, proof("u", "server", "n"));
        assert_ne!(p, proof("t", "server", "m"));
    }

    /// The daemon's socket with the app's client on it: sign-in, commands, errors and events.
    #[cfg(all(unix, feature = "daemon"))]
    #[tokio::test]
    async fn app_runs_commands_on_the_daemon() {
        use std::sync::{Arc, Mutex};

        let dir = std::env::temp_dir().join(format!("pb-ipc-{}", crate::db::new_id()));
        let events: server::Events = tokio::sync::broadcast::channel(64).0;
        let state = crate::open_state(dir.clone(), server::ui_sink(events.clone())).unwrap();
        server::start(state.clone(), events.clone(), &dir).unwrap();

        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let on_event: client::EventSink = Arc::new(move |name, payload| g.lock().unwrap().push((name.to_owned(), payload)));
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let conn = client::Conn::open(&dir, on_event, move || {
            let _ = closed_tx.send(());
        })
        .await
        .unwrap();

        let no_chans: crate::api::ChanFactory = Arc::new(|_| None);
        let saved = conn.call("save_prompt", json!({ "name": "Greet", "content": "Say hi" }), &no_chans).await.unwrap();
        assert_eq!(saved["name"], "Greet");
        let list = conn.call("list_prompts", json!({}), &no_chans).await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        let err = conn.call("list_messages", json!({}), &no_chans).await.unwrap_err();
        assert!(err.contains("conversationId"), "{err}");

        // A chat turn on a missing conversation fails at once: its error arrives on the channel,
        // then the daemon drops the channel and the client forgets it.
        let chat = Arc::new(Mutex::new(Vec::new()));
        let c = chat.clone();
        let chat_chans: crate::api::ChanFactory = Arc::new(move |_| {
            let c = c.clone();
            Some(Arc::new(move |v: Value| {
                c.lock().unwrap().push(v);
                true
            }) as crate::api::ChanSend)
        });
        let args = json!({
            "conversationId": "missing",
            "modelRef": "none",
            "text": "hi",
            "attachmentIds": [],
            "onEvent": "__CHANNEL__:1",
        });
        conn.call("send_chat_message", args, &chat_chans).await.unwrap();
        for _ in 0..200 {
            if conn.open_channels() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(conn.open_channels(), 0);
        assert!(chat.lock().unwrap().iter().any(|e| e["type"] == "error"), "{:?}", chat.lock().unwrap());

        (server::ui_sink(events.clone()))("test://ping", json!(7));
        for _ in 0..100 {
            if !got.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(got.lock().unwrap().first(), Some(&("test://ping".to_owned(), json!(7))));

        // A client without the token gets nothing.
        let wrong = dir.join("wrong");
        std::fs::create_dir_all(&wrong).unwrap();
        std::fs::write(wrong.join(TOKEN_FILE), "nope").unwrap();
        std::os::unix::fs::symlink(socket_path(&dir), socket_path(&wrong)).unwrap();
        let refused = client::Conn::open(&wrong, Arc::new(|_, _| {}), || {}).await;
        assert!(matches!(refused, Err(client::AttachError::Other(_))), "{:?}", refused.err());

        drop(conn);
        drop(closed_rx);
        state.shutdown();
        server::cleanup(&dir);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Channel messages reach the channel passed as an argument, under the client's own number.
    #[cfg(unix)]
    #[tokio::test]
    async fn channel_messages_reach_the_callers_channel() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, BufReader};

        let dir = std::env::temp_dir().join(format!("pb-ipc-chan-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(TOKEN_FILE), "secret").unwrap();
        let listener = tokio::net::UnixListener::bind(socket_path(&dir)).unwrap();

        // A stand-in daemon: sign the client in, then answer one command with a channel message.
        let fake = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = tokio::io::split(s);
            let mut r = BufReader::new(r);
            let hello = read_frame(&mut r).await.unwrap();
            let mine = nonce();
            let p = proof("secret", "server", hello["hello"].as_str().unwrap());
            write_frame(&mut w, &json!({ "nonce": mine, "proof": p })).await.unwrap();
            let auth = read_frame(&mut r).await.unwrap();
            assert_eq!(auth["auth"].as_str().unwrap(), proof("secret", "client", &mine));
            write_frame(&mut w, &json!({ "ready": true, "version": VERSION })).await.unwrap();
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let chan: u64 = req["args"]["onEvent"].as_str().unwrap().strip_prefix("__CHANNEL__:").unwrap().parse().unwrap();
            write_frame(&mut w, &json!({ "chan": chan, "msg": "hi" })).await.unwrap();
            write_frame(&mut w, &json!({ "id": req["id"], "ok": null })).await.unwrap();
            write_frame(&mut w, &json!({ "chanEnd": chan })).await.unwrap();
            // Held open until the client has read everything.
            let mut rest = String::new();
            let _ = r.read_line(&mut rest).await;
        });

        let conn = client::Conn::open(&dir, Arc::new(|_, _| {}), || {}).await.unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let chans: crate::api::ChanFactory = Arc::new(move |id| {
            assert_eq!(id, "__CHANNEL__:42");
            let g = g.clone();
            Some(Arc::new(move |v: Value| {
                g.lock().unwrap().push(v);
                true
            }) as crate::api::ChanSend)
        });
        conn.call("send_chat_message", json!({ "onEvent": "__CHANNEL__:42" }), &chans).await.unwrap();
        assert_eq!(*got.lock().unwrap(), vec![json!("hi")]);
        // The stand-in keeps the socket open, so only `chanEnd` can clear the channel.
        for _ in 0..200 {
            if conn.open_channels() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(conn.open_channels(), 0);
        drop(conn);
        fake.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
