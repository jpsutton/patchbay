//! The WebSocket transport and message framing over an established [`Secure`] link.

use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::handshake::{Link, Secure};
use crate::wire::Msg;

pub const DEFAULT_PORT: u16 = 41890;

/// A mesh URL from what a user typed: `host`, `host:port`, `http(s)://…` or `ws(s)://…`.
pub fn normalize_url(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    if s.starts_with("ws://") || s.starts_with("wss://") {
        s.to_owned()
    } else if let Some(rest) = s.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = s.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if s.contains(':') {
        format!("ws://{s}")
    } else {
        format!("ws://{s}:{DEFAULT_PORT}")
    }
}

pub fn send(sec: &mut Secure, link: &Link, m: &Msg) -> Result<()> {
    let bytes = serde_json::to_vec(m)?;
    for f in sec.seal(&bytes)? {
        link.tx.send(f).map_err(|_| anyhow!("connection closed"))?;
    }
    Ok(())
}

pub async fn recv(sec: &mut Secure, link: &mut Link) -> Result<Option<Msg>> {
    loop {
        let Some(frame) = link.rx.recv().await else { return Ok(None) };
        if let Some(whole) = sec.open(&frame)? {
            return Ok(Some(serde_json::from_slice(&whole)?));
        }
    }
}

/// Frames over a WebSocket, pumped by two tasks that end with the socket.
pub fn ws_link<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> Link
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio_tungstenite::tungstenite::Message;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(f) = out_rx.recv().await {
            if sink.send(Message::binary(f)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            match m {
                Message::Binary(b) => {
                    if in_tx.send(b.to_vec()).is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    Link { tx: out_tx, rx: in_rx }
}

/// Dial `url`, giving up after `timeout`.
pub async fn dial(url: &str, timeout: std::time::Duration) -> Result<Link> {
    let (ws, _) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| anyhow!("timed out"))??;
    Ok(ws_link(ws))
}
