//! Moving an agent's own session state between nodes. Harness-specific: Patchbay's own agent
//! (one JSONL file per session) and Grok Build's session directory. When a harness has nothing portable, callers fall back to a hand-off
//! prompt built from the transcript (see `handoff`), which works with any ACP agent.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Grok Build keeps each session in `$GROK_HOME/sessions/<url-encoded cwd>/<session id>/`.
fn session_dir(grok_home: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let enc = urlencoding::encode(cwd);
    // Longer names use a hashed form we don't reproduce; those sessions use the fallback.
    if enc.len() > 255 {
        return None;
    }
    Some(grok_home.join("sessions").join(enc.as_ref()).join(session_id))
}

/// The session's state as a tar archive, or `None` when there is nothing to carry over. `home` is
/// Patchbay's data directory; `agent` the session's agent id.
pub fn export(home: &Path, agent: &str, cwd: &str, session_id: &str) -> Result<Option<Vec<u8>>> {
    if agent == "patchbay" {
        return native_export(home, session_id);
    }
    grok_export(&home.join("grok"), cwd, session_id)
}

/// Unpack exported state for a session now rooted at `new_cwd`, rewriting the old path inside.
pub fn import(home: &Path, agent: &str, old_cwd: &str, new_cwd: &str, session_id: &str, archive: &[u8]) -> Result<bool> {
    if agent == "patchbay" {
        return native_import(home, old_cwd, new_cwd, session_id, archive);
    }
    grok_import(&home.join("grok"), old_cwd, new_cwd, session_id, archive)
}

fn native_path(home: &Path, session_id: &str) -> PathBuf {
    patchbay_agent::sessions_dir(home).join(format!("{session_id}.jsonl"))
}

fn native_export(home: &Path, session_id: &str) -> Result<Option<Vec<u8>>> {
    let path = native_path(home, session_id);
    if !path.is_file() {
        return Ok(None);
    }
    let mut b = tar::Builder::new(Vec::new());
    b.append_path_with_name(&path, "session.jsonl")?;
    Ok(Some(b.into_inner()?))
}

fn native_import(home: &Path, old_cwd: &str, new_cwd: &str, session_id: &str, archive: &[u8]) -> Result<bool> {
    let mut ar = tar::Archive::new(archive);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().and_then(|n| n.to_str()) != Some("session.jsonl") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        let text = if old_cwd != new_cwd { rewrite(&text, old_cwd, new_cwd) } else { text };
        let path = native_path(home, session_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        return Ok(true);
    }
    Ok(false)
}

fn grok_export(grok_home: &Path, cwd: &str, session_id: &str) -> Result<Option<Vec<u8>>> {
    let Some(dir) = session_dir(grok_home, cwd, session_id).filter(|d| d.is_dir()) else {
        return Ok(None);
    };
    let mut b = tar::Builder::new(Vec::new());
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        // Lock files belong to the process that held them.
        if name.to_string_lossy().ends_with(".lock") || !entry.file_type()?.is_file() {
            continue;
        }
        b.append_path_with_name(entry.path(), &name)?;
    }
    Ok(Some(b.into_inner()?))
}

fn grok_import(grok_home: &Path, old_cwd: &str, new_cwd: &str, session_id: &str, archive: &[u8]) -> Result<bool> {
    let Some(dir) = session_dir(grok_home, new_cwd, session_id) else { return Ok(false) };
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    if let Some(parent) = dir.parent() {
        let marker = parent.join(".cwd");
        if !marker.exists() {
            std::fs::write(marker, new_cwd)?;
        }
    }
    let mut ar = tar::Archive::new(archive);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        // Flat archive of plain files only.
        let Some(file) = name.file_name() else { continue };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if old_cwd != new_cwd
            && let Ok(text) = String::from_utf8(bytes.clone())
        {
            bytes = rewrite(&text, old_cwd, new_cwd).into_bytes();
        }
        std::fs::write(dir.join(file), bytes).with_context(|| format!("write {}", file.to_string_lossy()))?;
    }
    Ok(true)
}

/// Replace the old project path with the new one, both plain and JSON-escaped (Windows paths).
fn rewrite(text: &str, old: &str, new: &str) -> String {
    let mut out = text.replace(old, new);
    let (old_j, new_j) = (old.replace('\\', "\\\\"), new.replace('\\', "\\\\"));
    if old_j != old {
        out = out.replace(&old_j, &new_j);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_rewrites_paths() {
        let base = std::env::temp_dir().join(format!("pb-agent-{}", crate::db::new_id()));
        let (a, b) = (base.join("a"), base.join("b"));
        let dir = session_dir(&a, "/home/x/proj", "s1").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chat_history.jsonl"), r#"{"cwd":"/home/x/proj/src"}"#).unwrap();
        std::fs::write(dir.join("chat_history.jsonl.lock"), "").unwrap();
        let tar = grok_export(&a, "/home/x/proj", "s1").unwrap().unwrap();
        assert!(grok_import(&b, "/home/x/proj", "/srv/proj", "s1", &tar).unwrap());
        let got = session_dir(&b, "/srv/proj", "s1").unwrap();
        assert_eq!(std::fs::read_to_string(got.join("chat_history.jsonl")).unwrap(), r#"{"cwd":"/srv/proj/src"}"#);
        assert!(!got.join("chat_history.jsonl.lock").exists());
        assert!(grok_export(&a, "/elsewhere", "s1").unwrap().is_none());
    }

    #[test]
    fn native_sessions_move_with_rewritten_paths() {
        let base = std::env::temp_dir().join(format!("pb-agent-{}", crate::db::new_id()));
        let (a, b) = (base.join("a"), base.join("b"));
        let path = native_path(&a, "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"t\":\"meta\",\"cwd\":\"/home/x/proj\"}\n").unwrap();
        let tar = export(&a, "patchbay", "/home/x/proj", "s1").unwrap().unwrap();
        assert!(import(&b, "patchbay", "/home/x/proj", "/srv/proj", "s1", &tar).unwrap());
        assert_eq!(std::fs::read_to_string(native_path(&b, "s1")).unwrap(), "{\"t\":\"meta\",\"cwd\":\"/srv/proj\"}\n");
        assert!(export(&a, "patchbay", "/x", "missing").unwrap().is_none());
    }
}
