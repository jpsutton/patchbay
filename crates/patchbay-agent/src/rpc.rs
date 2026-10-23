//! A JSON-RPC 2.0 peer over newline-delimited JSON, the framing ACP uses on stdio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// What the peer answered to one of our requests.
pub type Reply = Result<Value, RpcError>;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    fn to_json(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if let Some(d) = &self.data {
            e["data"] = d.clone();
        }
        e
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Our end of the connection: send notifications, make requests.
#[derive(Clone)]
pub struct Peer {
    out: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
    next_id: Arc<AtomicU64>,
}

impl Peer {
    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.out.send(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }

    /// Make a request and wait for the answer. `Err` also when the connection closes.
    pub async fn request(&self, method: &str, params: Value) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        if self.out.send(line).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError::internal("connection closed"));
        }
        rx.await.unwrap_or_else(|_| Err(RpcError::internal("connection closed")))
    }
}

/// An incoming message that wants handling.
pub enum Incoming {
    Request { method: String, params: Value, reply: Responder },
    Notification { method: String, params: Value },
}

/// Answers one incoming request.
pub struct Responder {
    id: Value,
    out: mpsc::UnboundedSender<String>,
}

impl Responder {
    pub fn send(self, result: Reply) {
        let msg = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": self.id, "result": v}),
            Err(e) => json!({"jsonrpc": "2.0", "id": self.id, "error": e.to_json()}),
        };
        let _ = self.out.send(msg.to_string());
    }
}

/// Run the connection: returns our [`Peer`] and the stream of incoming messages. The reader and
/// writer run as tasks on the current runtime and end when either side closes.
pub fn connect(
    reader: impl AsyncRead + Unpin + Send + 'static,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
) -> (Peer, mpsc::UnboundedReceiver<Incoming>) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let peer = Peer { out: out_tx.clone(), pending: Arc::default(), next_id: Arc::new(AtomicU64::new(1)) };

    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    let pending = peer.pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let method = v.get("method").and_then(Value::as_str).map(str::to_owned);
            match (method, v.get("id").cloned()) {
                (Some(method), Some(id)) if !id.is_null() => {
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    let reply = Responder { id: id.clone(), out: out_tx.clone() };
                    if in_tx.send(Incoming::Request { method, params, reply }).is_err() {
                        break;
                    }
                }
                (Some(method), _) => {
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    if in_tx.send(Incoming::Notification { method, params }).is_err() {
                        break;
                    }
                }
                (None, Some(id)) => {
                    let Some(tx) = id.as_u64().and_then(|id| pending.lock().unwrap().remove(&id)) else {
                        continue;
                    };
                    let reply = match v.get("error") {
                        Some(e) => Err(RpcError {
                            code: e.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                            message: e.get("message").and_then(Value::as_str).unwrap_or("error").to_owned(),
                            data: e.get("data").cloned(),
                        }),
                        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(reply);
                }
                _ => {}
            }
        }
        // The peer is gone: fail whatever still waits for an answer.
        pending.lock().unwrap().clear();
    });

    (peer, in_rx)
}
