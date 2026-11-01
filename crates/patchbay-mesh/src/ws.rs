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

/// How long one frame may take to go out. A peer that stops reading would otherwise block the
/// writer for good, and with it the socket and everything queued for it.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 1 } else { 60 });

/// Frames over a WebSocket, pumped by two tasks that end with the socket.
///
/// Neither task may outlive the connection: the socket stays open as long as either half is
/// held. The writer gives up on a frame the peer doesn't take within [`WRITE_TIMEOUT`] (then
/// sending on the link fails, so its user sees the connection gone). The reader stops when the
/// link's user drops it, and when the peer's side ends it stops the writer too.
pub fn ws_link<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> Link
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio_tungstenite::tungstenite::Message;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let peer_gone = tokio_util::sync::CancellationToken::new();
    let writer_stop = peer_gone.clone();
    tokio::spawn(async move {
        loop {
            // Frames still queued when the link's user drops it go out first.
            let f = tokio::select! {
                f = out_rx.recv() => f,
                _ = writer_stop.cancelled() => None,
            };
            let Some(f) = f else { break };
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::binary(f))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::debug!("mesh: a peer took no data for {}s; closing", WRITE_TIMEOUT.as_secs());
                    break;
                }
            }
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sink.close()).await;
    });
    tokio::spawn(async move {
        loop {
            let m = tokio::select! {
                m = stream.next() => m,
                // The link's user is done with it: no one reads what arrives.
                _ = in_tx.closed() => return,
            };
            match m {
                Some(Ok(Message::Binary(b))) => {
                    if in_tx.send(b.to_vec()).is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
        peer_gone.cancel();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// A peer that stops reading ends the link, so the socket doesn't linger with data queued.
    #[tokio::test]
    async fn a_peer_that_stops_reading_closes_the_link() {
        let (a, _peer) = tokio::io::duplex(1024);
        let link = ws_link(tokio_tungstenite::WebSocketStream::from_raw_socket(a, Role::Client, None).await);
        // `_peer` never reads: its buffer fills, and the writer stalls on the next frame.
        let gone = async {
            while link.tx.send(vec![0u8; 4096]).is_ok() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone).await.expect("the link should close");
    }
}
