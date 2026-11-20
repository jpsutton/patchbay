//! Agent-to-agent file transfer over the mesh (the `send_file` MCP tool).
//!
//! The sending agent calls the tool once. Everything after that runs in Patchbay, not in the
//! model: the sender's node registers a one-file *offer*, tells the target session's host about it
//! (`code.file_offer`), and returns. The target host then pulls the file in 4 MiB chunks
//! (`code.file_chunk`) in a background task, verifies size and SHA-256, and only then wakes the
//! target agent with a message that names the saved path. A failure goes back to the sending
//! agent as a message instead. No step needs a further tool call, so a long transfer cannot trip a
//! tool-call timeout and the model never drives the chunking.
//!
//! Every request is an ordinary mesh RPC addressed to one node, so it reaches nodes that have no
//! direct route to each other through the mesh's relay circuits (docs/design/mesh.md, "Relaying
//! through a third node"). The offer is bound to the target host's node id and to an unguessable
//! token, expires after an hour and is dropped when the target is done.
//!
//! Received files land in `<target session folder>/.patchbay/inbox/`, which gets a `.gitignore`
//! so a transfer never ends up in a commit. An existing file is never overwritten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::AppState;

/// Largest file one transfer carries.
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// How long an offer stays fetchable.
const OFFER_TTL: Duration = Duration::from_secs(60 * 60);
/// Longest note a sender may attach.
const MAX_NOTE: usize = 2 * 1024;

struct Offer {
    path: PathBuf,
    /// The only node that may fetch it: the target session's host.
    to_node: String,
    expires: Instant,
}

fn offers() -> &'static Mutex<HashMap<String, Offer>> {
    static OFFERS: OnceLock<Mutex<HashMap<String, Offer>>> = OnceLock::new();
    OFFERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_offer(path: PathBuf, to_node: &str) -> String {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let now = Instant::now();
    let mut map = offers().lock().unwrap();
    map.retain(|_, o| o.expires > now);
    map.insert(token.clone(), Offer { path, to_node: to_node.to_owned(), expires: now + OFFER_TTL });
    token
}

/// Forget an offer. Only the node it was made for may do it.
pub(crate) fn drop_offer(requester: &str, token: &str) {
    let mut map = offers().lock().unwrap();
    if map.get(token).is_some_and(|o| o.to_node == requester) {
        map.remove(token);
    }
}

/// Source side of `code.file_chunk`: one chunk of an offered file, for the node it was offered to.
pub(crate) async fn serve_chunk(requester: &str, p: &Value) -> Result<Value> {
    let token = p["token"].as_str().ok_or_else(|| anyhow!("missing token"))?;
    let path = {
        let map = offers().lock().unwrap();
        let offer = map
            .get(token)
            .filter(|o| o.expires > Instant::now() && o.to_node == requester)
            .ok_or_else(|| anyhow!("no such file offer"))?;
        offer.path.clone()
    };
    crate::mesh::node::read_chunk(path, p["offset"].as_u64().unwrap_or(0)).await
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await.context("cannot open the file")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Sending side, called from the MCP bridge with a path already confined to the session folder.
/// Returns as soon as the target host accepted the offer; the transfer continues in the background.
pub(crate) async fn send_file(
    state: &AppState,
    source_id: &str,
    target_id: &str,
    file: PathBuf,
    note: Option<&str>,
) -> Result<Value> {
    if source_id == target_id {
        bail!("a session cannot send a file to itself");
    }
    if note.is_some_and(|n| n.len() > MAX_NOTE) {
        bail!("note is longer than 2 KiB");
    }
    let target = state
        .db
        .get_code_session(target_id)?
        .ok_or_else(|| anyhow!("target Code session was not found"))?;
    if target.archived_at.is_some() {
        bail!("target Code session is archived");
    }
    let host = target.host_node.clone().ok_or_else(|| anyhow!("target session has no host"))?;
    let size = tokio::fs::metadata(&file).await.context("cannot read the file")?.len();
    if size > MAX_FILE_BYTES {
        bail!("the file is {size} bytes; the limit is {MAX_FILE_BYTES} bytes (1 GiB)");
    }
    let sha = sha256_file(&file).await?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let me = state.mesh.node_id().to_owned();
    let token = register_offer(file, &host);
    let params = json!({
        "sourceSessionId": source_id,
        "targetSessionId": target_id,
        "token": token,
        "name": name,
        "size": size,
        "sha256": sha,
        "note": note,
    });
    let accepted = if host == me {
        accept_offer(state, &me, &params).await
    } else {
        state.mesh.rpc(&host, "code.file_offer", params).await
    };
    let accepted = match accepted {
        Ok(v) => v,
        Err(e) => {
            drop_offer(&host, &token);
            return Err(e);
        }
    };
    Ok(json!({
        "targetSessionId": target_id,
        "targetTitle": target.title,
        "name": name,
        "bytes": size,
        "sha256": sha,
        "savedAs": accepted["savedAs"],
        "relativePath": accepted["relativePath"],
        "status": "transferring",
        "note": "The transfer runs in the background; do not call this tool again for the same file. \
                 The target agent is messaged when the file has arrived and been verified. \
                 If the transfer fails, you get a message from the target session.",
    }))
}

/// Target side of `code.file_offer` (`from` is the node that holds the file). Checks the offer,
/// reserves the destination, starts the pull in the background and returns.
pub(crate) async fn accept_offer(state: &AppState, from: &str, p: &Value) -> Result<Value> {
    let text = |k: &str| p[k].as_str().ok_or_else(|| anyhow!("missing {k}"));
    let source_id = text("sourceSessionId")?.to_owned();
    let target_id = text("targetSessionId")?.to_owned();
    let token = text("token")?.to_owned();
    let sha = text("sha256")?.to_ascii_lowercase();
    let size = p["size"].as_u64().ok_or_else(|| anyhow!("missing size"))?;
    let note = p["note"].as_str().filter(|n| !n.trim().is_empty()).map(|n| n.chars().take(MAX_NOTE).collect::<String>());
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad checksum");
    }
    if size > MAX_FILE_BYTES {
        bail!("the file is larger than 1 GiB");
    }
    if state.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("the target session is closing; send again after Patchbay reopens it");
    }
    let source = state
        .db
        .get_code_session(&source_id)?
        .ok_or_else(|| anyhow!("source Code session was not found"))?;
    if source.host_node.as_deref() != Some(from) || source.archived_at.is_some() {
        bail!("source session is not hosted by the sending mesh member");
    }
    let target = state
        .db
        .get_code_session(&target_id)?
        .ok_or_else(|| anyhow!("target Code session was not found"))?;
    if target.archived_at.is_some() {
        bail!("target Code session is archived");
    }
    if target.host_node.as_deref() != Some(state.mesh.node_id()) {
        bail!("target session is no longer hosted here");
    }
    if state.code.is_moving(&target_id) {
        bail!("target session is being moved to another computer");
    }
    let dir = inbox_dir(Path::new(&target.cwd))?;
    let (dest, file) = reserve(&dir, &sanitize_name(text("name")?)).await?;
    let saved_name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let relative = format!(".patchbay/inbox/{saved_name}");
    let saved_as = dest.to_string_lossy().into_owned();

    let st = state.clone();
    let from = from.to_owned();
    let (rel, abs) = (relative.clone(), saved_as.clone());
    tokio::spawn(async move {
        let result = pull(&st, &from, &token, file, &dest, size, &sha).await;
        // Let the holder forget the offer (best effort; it expires anyway).
        let me = st.mesh.node_id().to_owned();
        if from == me {
            drop_offer(&me, &token);
        } else {
            let _ = st.mesh.rpc(&from, "code.file_done", json!({ "token": token })).await;
        }
        match result {
            Ok(()) => {
                let mut msg = format!(
                    "I sent you a file. It is saved on your computer at {abs} (in your session folder: {rel}), \
                     {size} bytes, sha256 {sha}, verified."
                );
                if let Some(note) = note {
                    msg.push_str("\n\nNote from the sender: ");
                    msg.push_str(&note);
                }
                let msg = crate::teams::strip_forged_tags(&msg);
                if let Err(e) = crate::deliver_agent_message_local(&st, &from, &source_id, &target_id, &msg).await {
                    tracing::warn!("file transfer: the file arrived but the agent was not told: {e:#}");
                }
            }
            Err(e) => {
                let msg = format!(
                    "The file transfer of '{saved_name}' to your peer session {target_id} failed: {e:#}. \
                     Nothing was kept on their side. You may try send_file again."
                );
                if let Err(e) = crate::send_agent_message(&st, &me, &target_id, &source_id, &msg).await {
                    tracing::warn!("file transfer failed and the sender was not told: {e:#}");
                }
            }
        }
    });
    Ok(json!({ "accepted": true, "savedAs": saved_as, "relativePath": relative }))
}

/// Pull every chunk into `file`, checking size and checksum. A failure removes the partial file.
async fn pull(
    st: &AppState,
    from: &str,
    token: &str,
    mut file: tokio::fs::File,
    dest: &Path,
    size: u64,
    sha: &str,
) -> Result<()> {
    let result: Result<()> = async {
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        loop {
            let r = pull_chunk(st, from, token, offset).await?;
            let chunk = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or_default())?;
            offset += chunk.len() as u64;
            if offset > size {
                bail!("the file is larger than offered");
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            if r["eof"].as_bool().unwrap_or(true) || chunk.is_empty() {
                break;
            }
        }
        file.flush().await?;
        if offset != size {
            bail!("received {offset} bytes, {size} were offered");
        }
        if hex::encode(hasher.finalize()) != sha {
            bail!("checksum mismatch");
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        drop(file);
        let _ = tokio::fs::remove_file(dest).await;
    }
    result
}

async fn pull_chunk(st: &AppState, from: &str, token: &str, offset: u64) -> Result<Value> {
    let p = json!({ "token": token, "offset": offset });
    if from == st.mesh.node_id() {
        serve_chunk(from, &p).await
    } else {
        st.mesh.rpc(from, "code.file_chunk", p).await
    }
}

/// `<session folder>/.patchbay/inbox`, created if needed and kept out of git.
fn inbox_dir(cwd: &Path) -> Result<PathBuf> {
    crate::refuse_foreign_path(cwd, cwd, "the session folder", &patchbay_agent::OsLinks)?;
    let root = cwd.canonicalize().context("the target session folder does not exist")?;
    if !root.is_dir() {
        bail!("the target session folder is not a directory");
    }
    let dir = root.join(".patchbay").join("inbox");
    std::fs::create_dir_all(&dir)?;
    let real = dir.canonicalize()?;
    if !real.starts_with(&root) {
        bail!("the inbox folder resolves outside the session folder");
    }
    let ignore = real.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    Ok(real)
}

/// One plain file name from a name a peer chose: no separator, no drive or stream, no hidden or
/// Windows device name, never empty. Everything outside `A-Z a-z 0-9 . - _` becomes `_`.
fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(100)
        .collect();
    out = out.trim_end_matches('.').to_owned();
    if !out.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        out.insert_str(0, "file_");
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_lowercase();
    let numbered = |p: &str| stem.len() == 4 && stem.starts_with(p) && stem.as_bytes()[3].is_ascii_digit();
    if matches!(stem.as_str(), "con" | "prn" | "aux" | "nul") || numbered("com") || numbered("lpt") {
        out.insert(0, '_');
    }
    out
}

/// Create the destination file under a name that is not taken (`a.txt`, `a-1.txt`, ...). Creation
/// fails on an existing file or link, so nothing is overwritten or followed.
async fn reserve(dir: &Path, name: &str) -> Result<(PathBuf, tokio::fs::File)> {
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_owned());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 0..1000u32 {
        let candidate = if n == 0 { dir.join(name) } else { dir.join(format!("{stem}-{n}{ext}")) };
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&candidate).await {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many files with that name in the inbox")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_one_plain_name() {
        assert_eq!(sanitize_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("C:\\Users\\x\\a b.txt"), "a_b.txt");
        assert_eq!(sanitize_name(".bashrc"), "file_.bashrc");
        assert_eq!(sanitize_name(""), "file_");
        assert_eq!(sanitize_name("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_name("name."), "name");
        assert!(!sanitize_name(&"a".repeat(500)).contains('/'));
        assert!(sanitize_name(&"a".repeat(500)).len() <= 100);
    }

    #[tokio::test]
    async fn reserve_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("pb-ft-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, _) = reserve(&dir, "a.txt").await.unwrap();
        let (b, _) = reserve(&dir, "a.txt").await.unwrap();
        assert_eq!(a.file_name().unwrap(), "a.txt");
        assert_eq!(b.file_name().unwrap(), "a-1.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offers_are_bound_to_the_target_node() {
        let token = register_offer(PathBuf::from("/tmp/x"), "node-b");
        drop_offer("node-c", &token);
        assert!(offers().lock().unwrap().contains_key(&token));
        drop_offer("node-b", &token);
        assert!(!offers().lock().unwrap().contains_key(&token));
    }

    #[tokio::test]
    async fn a_chunk_goes_only_to_the_node_it_was_offered_to() {
        let path = std::env::temp_dir().join(format!("pb-ft-{}.bin", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, b"hello").unwrap();
        let token = register_offer(path.clone(), "node-b");
        let p = json!({ "token": token, "offset": 0 });
        assert!(serve_chunk("node-c", &p).await.is_err());
        let r = serve_chunk("node-b", &p).await.unwrap();
        assert_eq!(r["eof"], true);
        assert_eq!(r["size"], 5);
        drop_offer("node-b", &token);
        assert!(serve_chunk("node-b", &p).await.is_err());
        let _ = std::fs::remove_file(&path);
    }
}
