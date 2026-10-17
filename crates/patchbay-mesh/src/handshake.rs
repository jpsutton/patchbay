//! Connection setup: SPAKE2 on the mesh passphrase, then Noise `XXpsk0` keyed by its result.
//! A remote member reconnecting to a gateway can instead present a resume ticket: Noise
//! `IKpsk2` keyed by the ticket, one round trip.
//!
//! SPAKE2 turns a memorable passphrase into a strong shared key without exposing it to offline
//! guessing: each connection attempt tests one guess. Noise then authenticates both nodes'
//! static keys, encrypts everything after, and binds the SPAKE2 messages through its prologue.
//! After the handshake, frames are Noise transport messages carrying JSON, fragmented to fit
//! Noise's 64 KiB message limit.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use tokio::sync::mpsc;

pub const NOISE_PARAMS: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";
pub const RESUME_PARAMS: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
const MAGIC: &[u8] = b"PBMESH1";
const SPAKE_ID: &[u8] = b"patchbay-mesh";
/// Sent in the clear by a responder whose first Noise message failed, so the dialer can tell a
/// wrong passphrase from a network failure.
const MISMATCH: &[u8] = b"PBMESH1!mismatch";
/// First frame of a resume: this, the ticket id, then the first Noise message.
const RESUME_MAGIC: &[u8] = b"PBRESUME1";
/// Sent in the clear when the ticket is unknown or its key doesn't match.
const RESUME_UNKNOWN: &[u8] = b"PBRESUME1!unknown";
pub const TICKET_ID_LEN: usize = 16;
const STEP_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest plaintext per Noise message, leaving room for the tag and our fragment flag.
const CHUNK: usize = 65_535 - 16 - 1;
/// Largest reassembled message.
const MAX_MESSAGE: usize = 256 * 1024 * 1024;

/// A bidirectional stream of opaque frames (a WebSocket, or a circuit through a relay).
pub struct Link {
    pub tx: mpsc::UnboundedSender<Vec<u8>>,
    pub rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl Link {
    /// Two links wired to each other (tests).
    #[doc(hidden)]
    pub fn pair() -> (Link, Link) {
        let (atx, brx) = mpsc::unbounded_channel();
        let (btx, arx) = mpsc::unbounded_channel();
        (Link { tx: atx, rx: arx }, Link { tx: btx, rx: brx })
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        match tokio::time::timeout(STEP_TIMEOUT, self.rx.recv()).await {
            Ok(Some(f)) => Ok(f),
            Ok(None) => bail!("connection closed during handshake"),
            Err(_) => bail!("handshake timed out"),
        }
    }

    fn send(&self, f: Vec<u8>) -> Result<()> {
        self.tx.send(f).map_err(|_| anyhow!("connection closed during handshake"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The other side doesn't share our passphrase (or isn't speaking our protocol).
    #[error("passphrase mismatch")]
    WrongPassphrase,
    /// The gateway doesn't know the resume ticket (expired, or it restarted).
    #[error("resume ticket not accepted")]
    UnknownTicket,
    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

pub struct Secure {
    pub transport: snow::TransportState,
    pub remote_static: [u8; 32],
    /// Reassembly buffer for fragmented messages.
    partial: Vec<u8>,
}

impl Secure {
    /// Encrypt one message into one or more frames.
    pub fn seal(&mut self, msg: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut chunks: Vec<&[u8]> = msg.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let mut buf = vec![0u8; 65_535];
        let mut frames = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            let mut plain = Vec::with_capacity(chunk.len() + 1);
            plain.push((i + 1 == chunks.len()) as u8);
            plain.extend_from_slice(chunk);
            let n = self.transport.write_message(&plain, &mut buf)?;
            frames.push(buf[..n].to_vec());
        }
        Ok(frames)
    }

    /// Decrypt one frame; returns a whole message once its last fragment arrives.
    pub fn open(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; frame.len()];
        let n = self.transport.read_message(frame, &mut buf)?;
        let (flag, body) = buf[..n].split_first().ok_or_else(|| anyhow!("empty frame"))?;
        if self.partial.len() + body.len() > MAX_MESSAGE {
            bail!("message too large");
        }
        self.partial.extend_from_slice(body);
        Ok((*flag == 1).then(|| std::mem::take(&mut self.partial)))
    }
}

fn spake(passphrase: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
    Spake2::<Ed25519Group>::start_symmetric(&Password::new(passphrase.as_bytes()), &SpakeId::new(SPAKE_ID))
}

fn prologue(init_msg: &[u8], resp_msg: &[u8]) -> Vec<u8> {
    let mut p = MAGIC.to_vec();
    p.extend_from_slice(&(init_msg.len() as u32).to_be_bytes());
    p.extend_from_slice(init_msg);
    p.extend_from_slice(resp_msg);
    p
}

fn psk(key: Vec<u8>) -> Result<[u8; 32]> {
    key.try_into().map_err(|_| anyhow!("unexpected SPAKE2 key length"))
}

fn strip_magic(frame: &[u8]) -> Result<&[u8]> {
    frame.strip_prefix(MAGIC).ok_or_else(|| anyhow!("not a Patchbay mesh peer"))
}

fn with_magic(msg: &[u8]) -> Vec<u8> {
    let mut f = MAGIC.to_vec();
    f.extend_from_slice(msg);
    f
}

/// Dialing side.
pub async fn initiate(link: &mut Link, passphrase: &str, static_private: &[u8; 32]) -> Result<Secure, HandshakeError> {
    let (state, my_msg) = spake(passphrase);
    link.send(with_magic(&my_msg))?;
    let their = link.recv().await?;
    let their_msg = strip_magic(&their)?.to_vec();
    let key = psk(state.finish(&their_msg).map_err(|_| HandshakeError::WrongPassphrase)?)?;
    let pro = prologue(&my_msg, &their_msg);
    let mut hs = snow::Builder::new(NOISE_PARAMS.parse().map_err(anyhow::Error::from)?)
        .local_private_key(static_private)
        .and_then(|b| b.psk(0, &key))
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_initiator())
        .map_err(anyhow::Error::from)?;
    let mut buf = vec![0u8; 65_535];
    let n = hs.write_message(&[], &mut buf).map_err(anyhow::Error::from)?;
    link.send(buf[..n].to_vec())?;
    let m2 = link.recv().await?;
    if m2 == MISMATCH {
        return Err(HandshakeError::WrongPassphrase);
    }
    hs.read_message(&m2, &mut buf).map_err(|_| HandshakeError::WrongPassphrase)?;
    let n = hs.write_message(&[], &mut buf).map_err(anyhow::Error::from)?;
    link.send(buf[..n].to_vec())?;
    finish(hs)
}

/// Accepting side.
pub async fn respond(link: &mut Link, passphrase: &str, static_private: &[u8; 32]) -> Result<Secure, HandshakeError> {
    let first = link.recv().await?;
    respond_passphrase(link, first, passphrase, static_private).await
}

/// Accepting side for either kind of opening. `lookup` gives a resume ticket's key; the result
/// carries the ticket id when the dialer resumed with one.
pub async fn respond_any(
    link: &mut Link,
    passphrase: &str,
    static_private: &[u8; 32],
    lookup: impl FnOnce(&[u8; TICKET_ID_LEN]) -> Option<[u8; 32]>,
) -> Result<(Secure, Option<[u8; TICKET_ID_LEN]>), HandshakeError> {
    let first = link.recv().await?;
    let Some(rest) = first.strip_prefix(RESUME_MAGIC) else {
        return Ok((respond_passphrase(link, first, passphrase, static_private).await?, None));
    };
    let refuse = |link: &Link| {
        let _ = link.send(RESUME_UNKNOWN.to_vec());
        HandshakeError::UnknownTicket
    };
    if rest.len() <= TICKET_ID_LEN {
        return Err(refuse(link));
    }
    let mut id = [0u8; TICKET_ID_LEN];
    id.copy_from_slice(&rest[..TICKET_ID_LEN]);
    let Some(key) = lookup(&id) else { return Err(refuse(link)) };
    let pro = resume_prologue(&id);
    let mut hs = snow::Builder::new(RESUME_PARAMS.parse().map_err(anyhow::Error::from)?)
        .local_private_key(static_private)
        .and_then(|b| b.psk(2, &key))
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_responder())
        .map_err(anyhow::Error::from)?;
    let mut buf = vec![0u8; 65_535];
    if hs.read_message(&rest[TICKET_ID_LEN..], &mut buf).is_err() {
        return Err(refuse(link));
    }
    let n = hs.write_message(&[], &mut buf).map_err(anyhow::Error::from)?;
    link.send(buf[..n].to_vec())?;
    Ok((finish(hs)?, Some(id)))
}

async fn respond_passphrase(
    link: &mut Link,
    first: Vec<u8>,
    passphrase: &str,
    static_private: &[u8; 32],
) -> Result<Secure, HandshakeError> {
    let their_msg = strip_magic(&first)?.to_vec();
    let (state, my_msg) = spake(passphrase);
    link.send(with_magic(&my_msg))?;
    let key = psk(state.finish(&their_msg).map_err(|_| HandshakeError::WrongPassphrase)?)?;
    let pro = prologue(&their_msg, &my_msg);
    let mut hs = snow::Builder::new(NOISE_PARAMS.parse().map_err(anyhow::Error::from)?)
        .local_private_key(static_private)
        .and_then(|b| b.psk(0, &key))
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_responder())
        .map_err(anyhow::Error::from)?;
    let mut buf = vec![0u8; 65_535];
    let m1 = link.recv().await?;
    // A wrong passphrase gives a different PSK, so the first Noise message fails to decrypt.
    if hs.read_message(&m1, &mut buf).is_err() {
        let _ = link.send(MISMATCH.to_vec());
        return Err(HandshakeError::WrongPassphrase);
    }
    let n = hs.write_message(&[], &mut buf).map_err(anyhow::Error::from)?;
    link.send(buf[..n].to_vec())?;
    let m3 = link.recv().await?;
    hs.read_message(&m3, &mut buf).map_err(|_| HandshakeError::WrongPassphrase)?;
    finish(hs)
}

fn resume_prologue(id: &[u8; TICKET_ID_LEN]) -> Vec<u8> {
    let mut p = RESUME_MAGIC.to_vec();
    p.extend_from_slice(id);
    p
}

/// Dialing side of a resume: one round trip to a gateway whose static key we already know.
pub async fn initiate_resume(
    link: &mut Link,
    static_private: &[u8; 32],
    gateway_static: &[u8; 32],
    ticket_id: &[u8; TICKET_ID_LEN],
    ticket_key: &[u8; 32],
) -> Result<Secure, HandshakeError> {
    let pro = resume_prologue(ticket_id);
    let mut hs = snow::Builder::new(RESUME_PARAMS.parse().map_err(anyhow::Error::from)?)
        .local_private_key(static_private)
        .and_then(|b| b.remote_public_key(gateway_static))
        .and_then(|b| b.psk(2, ticket_key))
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_initiator())
        .map_err(anyhow::Error::from)?;
    let mut buf = vec![0u8; 65_535];
    let n = hs.write_message(&[], &mut buf).map_err(anyhow::Error::from)?;
    let mut frame = resume_prologue(ticket_id);
    frame.extend_from_slice(&buf[..n]);
    link.send(frame)?;
    let m2 = link.recv().await?;
    if m2 == RESUME_UNKNOWN {
        return Err(HandshakeError::UnknownTicket);
    }
    hs.read_message(&m2, &mut buf).map_err(|_| HandshakeError::UnknownTicket)?;
    finish(hs)
}

fn finish(hs: snow::HandshakeState) -> Result<Secure, HandshakeError> {
    let remote: [u8; 32] = hs
        .get_remote_static()
        .ok_or_else(|| anyhow!("peer sent no static key"))?
        .try_into()
        .map_err(|_| anyhow!("bad static key"))?;
    Ok(Secure {
        transport: hs.into_transport_mode().map_err(anyhow::Error::from)?,
        remote_static: remote,
        partial: Vec::new(),
    })
}

/// Six-digit code both sides show while a join waits for approval: the same on both screens only
/// when nobody sits in the middle.
pub fn confirmation_code(a: &[u8; 32], b: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(b"patchbay-join-code");
    h.update(lo);
    h.update(hi);
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Identity;

    async fn run(pa: &str, pb: &str) -> (Result<Secure, HandshakeError>, Result<Secure, HandshakeError>) {
        let (mut la, mut lb) = Link::pair();
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (pa, pb) = (pa.to_owned(), pb.to_owned());
        let ta = tokio::spawn(async move { initiate(&mut la, &pa, &a.noise_private).await });
        let tb = tokio::spawn(async move { respond(&mut lb, &pb, &b.noise_private).await });
        (ta.await.unwrap(), tb.await.unwrap())
    }

    #[tokio::test]
    async fn same_passphrase_connects_and_round_trips() {
        let (a, b) = run("correct horse", "correct horse").await;
        let (mut a, mut b) = (a.unwrap(), b.unwrap());
        let big = vec![7u8; 200_000];
        let frames = a.seal(&big).unwrap();
        assert!(frames.len() > 3);
        let mut got = None;
        for f in frames {
            got = b.open(&f).unwrap().or(got);
        }
        assert_eq!(got.unwrap(), big);
        let f = b.seal(b"").unwrap();
        assert_eq!(a.open(&f[0]).unwrap().unwrap(), b"");
    }

    #[tokio::test]
    async fn wrong_passphrase_is_rejected() {
        let (a, b) = run("correct horse", "battery staple").await;
        assert!(matches!(b, Err(HandshakeError::WrongPassphrase)));
        assert!(matches!(a, Err(HandshakeError::WrongPassphrase)));
    }

    #[tokio::test]
    async fn resume_ticket_connects_in_one_round_trip() {
        let (mut la, mut lb) = Link::pair();
        let (phone, gw) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (id, key) = ([9u8; TICKET_ID_LEN], [4u8; 32]);
        let (gw_pub, phone_pub) = (gw.noise_public, phone.noise_public);
        let ta = tokio::spawn(async move { initiate_resume(&mut la, &phone.noise_private, &gw_pub, &id, &key).await });
        let tb = tokio::spawn(async move {
            respond_any(&mut lb, "unused passphrase", &gw.noise_private, |t| (t == &id).then_some(key)).await
        });
        let (mut a, (mut b, used)) = (ta.await.unwrap().unwrap(), tb.await.unwrap().unwrap());
        assert_eq!(used, Some(id));
        assert_eq!(b.remote_static, phone_pub);
        let f = a.seal(b"hi").unwrap();
        assert_eq!(b.open(&f[0]).unwrap().unwrap(), b"hi");
    }

    #[tokio::test]
    async fn unknown_or_wrong_ticket_is_refused() {
        for known in [None, Some([5u8; 32])] {
            let (mut la, mut lb) = Link::pair();
            let (phone, gw) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let gw_pub = gw.noise_public;
            let ta = tokio::spawn(async move {
                initiate_resume(&mut la, &phone.noise_private, &gw_pub, &[1; TICKET_ID_LEN], &[4; 32]).await
            });
            let tb = tokio::spawn(async move { respond_any(&mut lb, "p", &gw.noise_private, |_| known).await });
            assert!(matches!(ta.await.unwrap(), Err(HandshakeError::UnknownTicket)));
            match (known, tb.await.unwrap()) {
                (None, r) => assert!(matches!(r, Err(HandshakeError::UnknownTicket))),
                // The key enters at message 2, so only the dialer sees a wrong one; the gateway
                // finds out when the dialer's first message fails to decrypt.
                (Some(_), Ok((mut b, _))) => assert!(b.open(&[0u8; 32]).is_err()),
                (Some(_), Err(_)) => {}
            }
        }
    }

    #[tokio::test]
    async fn respond_any_still_takes_the_passphrase() {
        let (mut la, mut lb) = Link::pair();
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let ta = tokio::spawn(async move { initiate(&mut la, "correct horse", &a.noise_private).await });
        let tb = tokio::spawn(async move { respond_any(&mut lb, "correct horse", &b.noise_private, |_| None).await });
        assert!(ta.await.unwrap().is_ok());
        assert!(tb.await.unwrap().unwrap().1.is_none());
    }

    #[test]
    fn codes_match_on_both_sides() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_eq!(confirmation_code(&a, &b), confirmation_code(&b, &a));
        assert_eq!(confirmation_code(&a, &b).len(), 7);
    }
}
