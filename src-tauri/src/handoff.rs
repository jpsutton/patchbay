//! Moving a Code session's working state between nodes: the workspace (as git objects, or a tar
//! of a non-git folder) and the agent's own session state.
//!
//! The source packages a snapshot into an outbox directory; the target fetches its files over
//! the mesh and unpacks them into its own checkout. Uncommitted work travels as a commit made
//! with a private index (`refs/patchbay/<kind>/<session>`), so the user's branch and index stay
//! untouched on the source. Checkpoints use the same package, but a receiver keeps them in a
//! bare mirror repository until a "take over" needs them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::code::agent_state;
use crate::db::CodeSession;

/// Largest non-git folder we'll tar up.
const MAX_TARBALL: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitMeta {
    pub branch: Option<String>,
    /// The source's HEAD commit.
    pub head: String,
    /// Snapshot commit: HEAD plus every uncommitted and untracked (not ignored) file.
    pub commit: String,
    /// Ref the bundle carries the snapshot under.
    pub reference: String,
    pub remote: Option<String>,
    /// Session folder relative to the repository root ("" when it is the root).
    pub subpath: String,
    /// Whether `workspace.bundle` exists (not needed when the target has the commit already).
    pub bundle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub session_id: String,
    pub cwd: String,
    pub acp_session_id: Option<String>,
    pub git: Option<GitMeta>,
    /// A non-git folder, as `workspace.tar`.
    pub tarball: bool,
    /// Agent state, as `agent.tar`.
    pub agent: bool,
    pub files: Vec<String>,
    pub created_at: i64,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, &[], args)
}

fn git_env(dir: &Path, env: &[(&str, &std::ffi::OsStr)], args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn outbox(data_dir: &Path) -> PathBuf {
    data_dir.join("mesh").join("outbox")
}

pub fn inbox(data_dir: &Path) -> PathBuf {
    data_dir.join("mesh").join("inbox")
}

fn checkpoint_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("mesh").join("checkpoints").join(session_id)
}

fn mirror_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("mesh").join("mirrors").join(format!("{session_id}.git"))
}

/// Commit the working tree as it is (tracked changes and untracked, non-ignored files) on top
/// of HEAD, without touching the real index or branch. `None` when `cwd` isn't in a git repo
/// with at least one commit.
fn snapshot(cwd: &Path, reference: &str, scratch: &Path) -> Result<Option<(PathBuf, GitMeta)>> {
    let Ok(top) = git(cwd, &["rev-parse", "--show-toplevel"]) else { return Ok(None) };
    let top = PathBuf::from(top);
    let Ok(head) = git(&top, &["rev-parse", "HEAD"]) else { return Ok(None) };
    let index = scratch.join("snapshot.index");
    let _ = std::fs::remove_file(&index);
    let env: [(&str, &std::ffi::OsStr); 1] = [("GIT_INDEX_FILE", index.as_os_str())];
    git_env(&top, &env, &["read-tree", "HEAD"])?;
    git_env(&top, &env, &["add", "-A"])?;
    let tree = git_env(&top, &env, &["write-tree"])?;
    let _ = std::fs::remove_file(&index);
    let who: [(&str, &std::ffi::OsStr); 4] = [
        ("GIT_AUTHOR_NAME", "Patchbay".as_ref()),
        ("GIT_AUTHOR_EMAIL", "patchbay@localhost".as_ref()),
        ("GIT_COMMITTER_NAME", "Patchbay".as_ref()),
        ("GIT_COMMITTER_EMAIL", "patchbay@localhost".as_ref()),
    ];
    let commit = git_env(&top, &who, &["commit-tree", &tree, "-p", &head, "-m", "Patchbay snapshot of uncommitted work"])?;
    git(&top, &["update-ref", reference, &commit])?;
    let branch = git(&top, &["symbolic-ref", "--short", "-q", "HEAD"]).ok().filter(|b| !b.is_empty());
    let remote = git(&top, &["remote", "get-url", "origin"]).ok().filter(|r| !r.is_empty());
    let subpath = cwd
        .canonicalize()
        .ok()
        .and_then(|c| top.canonicalize().ok().and_then(|t| c.strip_prefix(t).ok().map(|p| p.to_string_lossy().into_owned())))
        .unwrap_or_default();
    Ok(Some((top, GitMeta { branch, head, commit, reference: reference.to_owned(), remote, subpath, bundle: false })))
}

/// Package session state into a fresh outbox directory. `have`: commits the receiver already
/// has, left out of the bundle.
pub fn package(
    data_dir: &Path,
    grok_home: &Path,
    s: &CodeSession,
    kind: &str,
    have: &[String],
) -> Result<(PathBuf, Meta)> {
    let dir = outbox(data_dir).join(format!("{kind}-{}-{}", s.id, crate::db::new_id()));
    std::fs::create_dir_all(&dir)?;
    let cwd = Path::new(&s.cwd);
    let mut files = Vec::new();
    let reference = format!("refs/patchbay/{kind}/{}", s.id);
    let git_meta = match snapshot(cwd, &reference, &dir)? {
        Some((top, mut g)) => {
            let mut args: Vec<String> = vec!["bundle".into(), "create".into(), dir.join("workspace.bundle").to_string_lossy().into_owned(), reference.clone()];
            let mut known = Vec::new();
            for h in have {
                if git(&top, &["cat-file", "-e", &format!("{h}^{{commit}}")]).is_ok() {
                    known.push(h.clone());
                }
            }
            let has_snapshot = known.iter().any(|h| git(&top, &["merge-base", "--is-ancestor", &g.commit, h]).is_ok());
            if !has_snapshot {
                args.extend(known.iter().map(|h| format!("^{h}")));
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                git(&top, &refs)?;
                g.bundle = true;
                files.push("workspace.bundle".to_owned());
            }
            Some(g)
        }
        None => None,
    };
    let mut tarball = false;
    if git_meta.is_none() {
        if !cwd.is_dir() {
            bail!("{} doesn't exist on this computer", s.cwd);
        }
        let size = dir_size(cwd)?;
        if size > MAX_TARBALL {
            bail!("{} isn't a git repository and is too large ({} MB) to copy", s.cwd, size / 1_048_576);
        }
        let f = std::fs::File::create(dir.join("workspace.tar"))?;
        let mut b = tar::Builder::new(f);
        b.follow_symlinks(false);
        b.append_dir_all(".", cwd)?;
        b.finish()?;
        tarball = true;
        files.push("workspace.tar".to_owned());
    }
    let mut agent = false;
    if let Some(sid) = &s.acp_session_id
        && let Some(bytes) = agent_state::export(grok_home, &s.cwd, sid)?
    {
        std::fs::write(dir.join("agent.tar"), bytes)?;
        agent = true;
        files.push("agent.tar".to_owned());
    }
    let meta = Meta {
        session_id: s.id.clone(),
        cwd: s.cwd.clone(),
        acp_session_id: s.acp_session_id.clone(),
        git: git_meta,
        tarball,
        agent,
        files,
        created_at: crate::db::now_ms(),
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    Ok((dir, meta))
}

fn dir_size(p: &Path) -> Result<u64> {
    let mut total = 0;
    for e in walk(p)? {
        total += e.metadata().map(|m| m.len()).unwrap_or(0);
        if total > MAX_TARBALL {
            break;
        }
    }
    Ok(total)
}

fn walk(p: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                stack.push(e.path());
            } else {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Where a session folder lands in the target repository root.
fn repo_root(target_cwd: &Path, subpath: &str) -> PathBuf {
    if subpath.is_empty() {
        return target_cwd.to_path_buf();
    }
    let depth = Path::new(subpath).components().count();
    target_cwd.ancestors().nth(depth).unwrap_or(target_cwd).to_path_buf()
}

/// Bring `target_cwd` to the snapshot: fetch it from `source` (a bundle file or a mirror repo),
/// put the branch at the source's HEAD and lay the uncommitted work on top, unstaged.
/// Returns a note for the user when the branch had to be renamed.
pub fn apply_git(source: &Path, g: &GitMeta, target_cwd: &Path, from_name: &str) -> Result<Option<String>> {
    let root = repo_root(target_cwd, &g.subpath);
    let fresh = !root.exists() || std::fs::read_dir(&root).map(|mut d| d.next().is_none()).unwrap_or(true);
    if fresh {
        std::fs::create_dir_all(&root)?;
        git(&root, &["init", "-q"])?;
    } else {
        if git(&root, &["rev-parse", "--show-toplevel"]).is_err() {
            bail!("{} exists but isn't a git repository", root.display());
        }
        let dirty = git(&root, &["status", "--porcelain"])?;
        if !dirty.is_empty() {
            bail!("{} has uncommitted changes; commit or stash them first", root.display());
        }
    }
    let src = source.to_string_lossy();
    git(&root, &["fetch", "-q", &src, &format!("+{r}:{r}", r = g.reference)])?;
    if fresh
        && let Some(r) = &g.remote
    {
        let _ = git(&root, &["remote", "add", "origin", r]);
    }
    let mut note = None;
    match &g.branch {
        Some(b) => {
            let exists = git(&root, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]).is_ok();
            let fast_forward = !exists || git(&root, &["merge-base", "--is-ancestor", b, &g.head]).is_ok();
            let name = if fast_forward {
                b.clone()
            } else {
                let alt = format!("{b}-from-{}", slug(from_name));
                note = Some(format!("Your {b} branch here has other commits, so the session continues on {alt}."));
                alt
            };
            git(&root, &["checkout", "-q", "-B", &name, &g.head])?;
        }
        None => {
            git(&root, &["checkout", "-q", "--detach", &g.head])?;
        }
    }
    // Working tree = snapshot; index = HEAD, so the changes show as uncommitted, as on the source.
    git(&root, &["read-tree", "-u", "--reset", &g.commit])?;
    git(&root, &["reset", "-q"])?;
    Ok(note)
}

fn slug(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    out.trim_matches('-').to_owned()
}

pub fn apply_tarball(tar_path: &Path, target_cwd: &Path) -> Result<()> {
    if target_cwd.exists() && std::fs::read_dir(target_cwd)?.next().is_some() {
        bail!("{} isn't empty", target_cwd.display());
    }
    std::fs::create_dir_all(target_cwd)?;
    tar::Archive::new(std::fs::File::open(tar_path)?).unpack(target_cwd)?;
    Ok(())
}

/// Unpack a fetched package into `target_cwd`. Returns (agent state restored, note for the user).
pub fn unpack(dir: &Path, meta: &Meta, grok_home: &Path, target_cwd: &Path, from_name: &str) -> Result<(bool, Option<String>)> {
    let mut note = None;
    if let Some(g) = &meta.git {
        if !g.bundle {
            bail!("the package has no workspace bundle");
        }
        note = apply_git(&dir.join("workspace.bundle"), g, target_cwd, from_name)?;
    } else if meta.tarball {
        apply_tarball(&dir.join("workspace.tar"), target_cwd)?;
    }
    let restored = restore_agent(dir, meta, grok_home, target_cwd)?;
    Ok((restored, note))
}

fn restore_agent(dir: &Path, meta: &Meta, grok_home: &Path, target_cwd: &Path) -> Result<bool> {
    match (&meta.acp_session_id, meta.agent) {
        (Some(sid), true) => {
            let bytes = std::fs::read(dir.join("agent.tar"))?;
            agent_state::import(grok_home, &meta.cwd, &target_cwd.to_string_lossy(), sid, &bytes)
        }
        _ => Ok(false),
    }
}

/// Keep a received checkpoint: git objects into this session's mirror, the rest beside it.
pub fn store_checkpoint(data_dir: &Path, dir: &Path, meta: &Meta) -> Result<()> {
    let keep = checkpoint_dir(data_dir, &meta.session_id);
    std::fs::create_dir_all(&keep)?;
    if let Some(g) = &meta.git
        && g.bundle
    {
        let mirror = mirror_dir(data_dir, &meta.session_id);
        if !mirror.exists() {
            std::fs::create_dir_all(&mirror)?;
            git(&mirror, &["init", "-q", "--bare"])?;
        }
        let src = dir.join("workspace.bundle");
        git(&mirror, &["fetch", "-q", &src.to_string_lossy(), &format!("+{r}:{r}", r = g.reference)])?;
    }
    for f in ["agent.tar", "workspace.tar"] {
        let p = dir.join(f);
        let dest = keep.join(f);
        if p.exists() {
            std::fs::copy(&p, &dest)?;
        } else if meta.files.iter().all(|x| x != f) {
            let _ = std::fs::remove_file(&dest);
        }
    }
    std::fs::write(keep.join("meta.json"), serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

/// The latest checkpoint this node holds for a session.
pub fn checkpoint(data_dir: &Path, session_id: &str) -> Option<Meta> {
    let raw = std::fs::read(checkpoint_dir(data_dir, session_id).join("meta.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Commits this node already holds for a session's checkpoints (sent as `have`).
pub fn checkpoint_have(data_dir: &Path, session_id: &str) -> Vec<String> {
    let Some(meta) = checkpoint(data_dir, session_id) else { return Vec::new() };
    let Some(g) = meta.git else { return Vec::new() };
    let mirror = mirror_dir(data_dir, session_id);
    git(&mirror, &["rev-parse", "-q", "--verify", &g.reference]).map(|c| vec![c]).unwrap_or_default()
}

/// Restore a session from the local checkpoint. Returns (agent state restored, note).
pub fn restore_checkpoint(data_dir: &Path, grok_home: &Path, session_id: &str, target_cwd: &Path, from_name: &str) -> Result<(bool, Option<String>)> {
    let meta = checkpoint(data_dir, session_id).ok_or_else(|| anyhow!("no checkpoint of this session on this computer"))?;
    let keep = checkpoint_dir(data_dir, session_id);
    let mut note = None;
    if let Some(g) = &meta.git {
        note = apply_git(&mirror_dir(data_dir, session_id), g, target_cwd, from_name)?;
    } else if meta.tarball {
        apply_tarball(&keep.join("workspace.tar"), target_cwd)?;
    }
    let restored = restore_agent(&keep, &meta, grok_home, target_cwd)?;
    Ok((restored, note))
}

/// Save work a session did here after it moved elsewhere (we were offline when it was taken
/// over), so nothing is silently lost. Returns the ref name.
pub fn keep_orphaned_work(cwd: &Path, session_id: &str) -> Result<Option<String>> {
    let reference = format!("refs/patchbay/orphaned/{session_id}-{}", crate::db::now_ms());
    let scratch = std::env::temp_dir().join(format!("pb-orphan-{}", crate::db::new_id()));
    std::fs::create_dir_all(&scratch)?;
    let r = snapshot(cwd, &reference, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(r?.map(|_| reference))
}

/// What the agent gets as context when its own state couldn't move with the session.
pub fn handoff_prompt(transcript_json: Option<&str>, from_name: &str) -> String {
    const MAX: usize = 24_000;
    let mut lines = Vec::new();
    if let Some(raw) = transcript_json
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw)
    {
        for e in v["entries"].as_array().into_iter().flatten() {
            let text = e["text"].as_str().unwrap_or_default().trim();
            match e["kind"].as_str() {
                Some("user") if !text.is_empty() => lines.push(format!("User: {text}")),
                Some("agent") if !text.is_empty() => lines.push(format!("Assistant: {text}")),
                _ => {}
            }
        }
    }
    let mut body = lines.join("\n\n");
    if body.len() > MAX {
        let mut cut = body.len() - MAX;
        while !body.is_char_boundary(cut) {
            cut += 1;
        }
        body = format!("[earlier conversation omitted]\n\n{}", &body[cut..]);
    }
    format!(
        "This session was moved here from {from_name}. Your earlier session state couldn't come along, \
         so here is the conversation so far for context. The project files are as they were on \
         {from_name}, including uncommitted changes.\n\n<previous-conversation>\n{body}\n</previous-conversation>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        git(dir, args).unwrap();
    }

    fn session(cwd: &Path) -> CodeSession {
        CodeSession {
            id: "sess1".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            acp_session_id: None,
            title: "t".into(),
            model_ref: None,
            pinned: false,
            created_at: 0,
            updated_at: 0,
            activity: None,
            host_node: None,
            epoch: 0,
        }
    }

    #[test]
    fn moves_uncommitted_work_to_another_checkout() {
        let base = std::env::temp_dir().join(format!("pb-handoff-{}", crate::db::new_id()));
        let (src, dst, data) = (base.join("src"), base.join("dst"), base.join("data"));
        std::fs::create_dir_all(&src).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        sh(&src, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "root"]);
        std::fs::write(src.join("a.txt"), "committed").unwrap();
        sh(&src, &["add", "a.txt"]);
        sh(&src, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "a"]);
        std::fs::write(src.join("a.txt"), "edited, not committed").unwrap();
        std::fs::write(src.join("new.txt"), "untracked").unwrap();
        std::fs::write(src.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(src.join("ignored.txt"), "stay home").unwrap();

        let (dir, meta) = package(&data, &data.join("grok"), &session(&src), "handoff", &[]).unwrap();
        // The source is untouched.
        assert!(git(&src, &["status", "--porcelain"]).unwrap().contains("a.txt"));
        assert_eq!(git(&src, &["diff", "--cached", "--name-only"]).unwrap(), "");

        let (restored, note) = unpack(&dir, &meta, &data.join("grok2"), &dst, "alpha").unwrap();
        assert!(!restored);
        assert!(note.is_none());
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "edited, not committed");
        assert_eq!(std::fs::read_to_string(dst.join("new.txt")).unwrap(), "untracked");
        assert!(!dst.join("ignored.txt").exists());
        assert_eq!(git(&dst, &["symbolic-ref", "--short", "HEAD"]).unwrap(), "main");
        assert_eq!(git(&dst, &["rev-parse", "HEAD"]).unwrap(), meta.git.as_ref().unwrap().head);
        // Changes arrive uncommitted and unstaged.
        assert_eq!(git(&dst, &["diff", "--cached", "--name-only"]).unwrap(), "");
        assert!(git(&dst, &["status", "--porcelain"]).unwrap().contains("M a.txt"));

        // A checkpoint stored in a mirror can restore it too, and later ones are incremental.
        store_checkpoint(&data, &dir, &meta).unwrap();
        let have = checkpoint_have(&data, "sess1");
        assert_eq!(have.len(), 1);
        std::fs::write(src.join("a.txt"), "second edit").unwrap();
        let (dir2, meta2) = package(&data, &data.join("grok"), &session(&src), "handoff", &have).unwrap();
        store_checkpoint(&data, &dir2, &meta2).unwrap();
        let dst2 = base.join("dst2");
        restore_checkpoint(&data, &data.join("grok3"), "sess1", &dst2, "alpha").unwrap();
        assert_eq!(std::fs::read_to_string(dst2.join("a.txt")).unwrap(), "second edit");
    }

    #[test]
    fn refuses_a_dirty_target() {
        let base = std::env::temp_dir().join(format!("pb-handoff-{}", crate::db::new_id()));
        let (src, dst, data) = (base.join("src"), base.join("dst"), base.join("data"));
        for d in [&src, &dst] {
            std::fs::create_dir_all(d).unwrap();
            sh(d, &["init", "-q"]);
            sh(d, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "root"]);
        }
        std::fs::write(dst.join("mine.txt"), "local work").unwrap();
        let (dir, meta) = package(&data, &data.join("grok"), &session(&src), "handoff", &[]).unwrap();
        let err = unpack(&dir, &meta, &data.join("g"), &dst, "alpha").unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
    }

    #[test]
    fn handoff_prompt_carries_the_conversation() {
        let t = r#"{"v":1,"entries":[{"kind":"user","text":"fix the bug"},{"kind":"tool","id":"x"},{"kind":"agent","text":"done"}]}"#;
        let p = handoff_prompt(Some(t), "alpha");
        assert!(p.contains("User: fix the bug") && p.contains("Assistant: done") && p.contains("from alpha"));
    }
}
