//! Subagents: other model loops the main agent starts, each in a session of its own.
//!
//! A subagent has its own history, files, shells and (optionally) its own git worktree. It runs
//! in the turn that started it, next to other subagents, or in the background; a background
//! one reports through a notification the main agent reads on its next step.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use patchbay_llm::{Message, Part, Request, Role, SystemBlock};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::prompt;
use crate::prompts::Prompts;
use crate::session::Session;
use crate::store::Entry;
use crate::tools::Registry;
use crate::turn;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Read-only search and reading.
    Explore,
    /// Starts fresh with every tool but the ones that talk to the user or start subagents.
    General,
    /// Starts with the main agent's conversation as reference and does one directive.
    Fork,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Explore => "explore",
            Kind::General => "general-purpose",
            Kind::Fork => "fork",
        }
    }
}

/// A running subagent that has done nothing for this long counts as stalled. A model call that
/// streams, or a tool that finishes, counts as doing something.
pub const STALL_AFTER: Duration = Duration::from_secs(300);
/// The same for one waiting on the user (a permission prompt): shorter, because only the user can
/// get it going again.
pub const BLOCKED_AFTER: Duration = Duration::from_secs(120);

/// What a session is doing right now, so anyone outside it can tell a busy one from a stuck one.
pub struct Liveness {
    started: Instant,
    inner: Mutex<Live>,
}

struct Live {
    last: Instant,
    steps: u32,
    current: String,
    /// Waiting for the user, not for the model or a tool.
    blocked: bool,
    /// Already reported to the main agent as stalled; cleared when it moves again.
    reported: bool,
}

/// One look at a [`Liveness`].
#[derive(Debug, Clone)]
pub struct Pulse {
    pub elapsed: Duration,
    pub idle: Duration,
    pub steps: u32,
    pub current: String,
    pub blocked: bool,
}

impl Default for Liveness {
    fn default() -> Self {
        let now = Instant::now();
        Liveness { started: now, inner: Mutex::new(Live { last: now, steps: 0, current: "starting".into(), blocked: false, reported: false }) }
    }
}

impl Liveness {
    /// Something happened (a stream event, a tool result): it is alive.
    pub fn touch(&self) {
        let mut l = self.inner.lock().unwrap();
        l.last = Instant::now();
        l.reported = false;
    }

    /// Now doing `what` (a model call, a tool).
    pub fn set(&self, what: impl Into<String>) {
        let mut l = self.inner.lock().unwrap();
        l.current = what.into();
        l.blocked = false;
        l.last = Instant::now();
        l.reported = false;
    }

    /// Now waiting for the user to answer `what`.
    pub fn block(&self, what: impl Into<String>) {
        let mut l = self.inner.lock().unwrap();
        l.current = what.into();
        l.blocked = true;
        l.last = Instant::now();
        l.reported = false;
    }

    /// A model request is about to go out.
    pub fn step(&self) {
        self.inner.lock().unwrap().steps += 1;
    }

    pub fn pulse(&self) -> Pulse {
        let l = self.inner.lock().unwrap();
        Pulse { elapsed: self.started.elapsed(), idle: l.last.elapsed(), steps: l.steps, current: l.current.clone(), blocked: l.blocked }
    }

    /// The pulse, once, when it has been quiet for too long.
    fn take_stall(&self) -> Option<Pulse> {
        let mut l = self.inner.lock().unwrap();
        let limit = if l.blocked { BLOCKED_AFTER } else { STALL_AFTER };
        if l.reported || l.last.elapsed() < limit {
            return None;
        }
        l.reported = true;
        Some(Pulse { elapsed: self.started.elapsed(), idle: l.last.elapsed(), steps: l.steps, current: l.current.clone(), blocked: l.blocked })
    }
}

/// `75s`, `3m 05s`, `1h 02m`.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed(String),
    Stopped,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "completed",
            Status::Failed(_) => "failed",
            Status::Stopped => "stopped",
        }
    }
}

/// A git worktree a subagent works in, so its edits stay apart from the main checkout.
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    repo: PathBuf,
    base: String,
}

pub struct SubAgent {
    pub id: String,
    pub name: Option<String>,
    pub kind: Kind,
    pub description: String,
    pub child: Arc<Session>,
    pub system: Vec<SystemBlock>,
    pub registry: Registry,
    pub status: Mutex<Status>,
    /// Messages for it to read before its next request.
    pub inbox: Mutex<Vec<String>>,
    pub cancel: Mutex<CancellationToken>,
    pub worktree: Mutex<Option<Worktree>>,
    pub background: AtomicBool,
}

impl SubAgent {
    /// One line on how it is doing: id, name, kind, state, what it is for.
    pub fn summary(&self) -> String {
        let name = self.name.as_deref().map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let status = self.status.lock().unwrap().clone();
        let state = match &status {
            Status::Running => {
                let p = self.child.live.pulse();
                format!("running {}, idle {}, {} steps: {}", span(p.elapsed), span(p.idle), p.steps, p.current)
            }
            Status::Failed(e) => format!("failed: {}", e.lines().next().unwrap_or("")),
            other => other.label().to_owned(),
        };
        format!("{}{name} ({}) {state} [{}]", self.id, self.kind.label(), self.description)
    }

    fn status_json(&self) -> serde_json::Value {
        let p = self.child.live.pulse();
        let status = self.status.lock().unwrap().clone();
        let worktree = self.worktree.lock().unwrap().as_ref().map(|w| serde_json::json!({"path": w.path, "branch": w.branch}));
        let error = if let Status::Failed(e) = &status { Some(e.clone()) } else { None };
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind.label(),
            "description": self.description,
            "status": status.label(),
            "error": error,
            "background": self.background.load(Ordering::SeqCst),
            "elapsed_secs": p.elapsed.as_secs(),
            "idle_secs": p.idle.as_secs(),
            "steps": p.steps,
            "current": p.current,
            "blocked": p.blocked,
            "worktree": worktree,
        })
    }
}

/// A finished background subagent, waiting for the main agent to read it.
pub struct Note {
    pub agent_id: String,
    pub description: String,
    pub status: &'static str,
    pub text: String,
}

#[derive(Default)]
pub struct Agents {
    list: Mutex<Vec<Arc<SubAgent>>>,
    notes: Mutex<Vec<Note>>,
    wake: Notify,
    next: AtomicU64,
}

impl Agents {
    pub fn next_id(&self) -> String {
        format!("agent_{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn add(&self, a: Arc<SubAgent>) {
        self.list.lock().unwrap().push(a);
    }

    /// An agent by id or name.
    pub fn find(&self, key: &str) -> Option<Arc<SubAgent>> {
        self.list.lock().unwrap().iter().find(|a| a.id == key || a.name.as_deref() == Some(key)).cloned()
    }

    pub fn name_taken(&self, name: &str) -> bool {
        self.list.lock().unwrap().iter().any(|a| a.name.as_deref() == Some(name) || a.id == name)
    }

    /// Background subagents still running.
    pub fn pending(&self) -> usize {
        self.list
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.background.load(Ordering::SeqCst) && *a.status.lock().unwrap() == Status::Running)
            .count()
    }

    pub fn push_note(&self, n: Note) {
        self.notes.lock().unwrap().push(n);
        self.wake.notify_one();
    }

    pub fn drain_notes(&self) -> Vec<Note> {
        std::mem::take(&mut *self.notes.lock().unwrap())
    }

    /// Resolves when a note arrives (or has arrived since the last wait).
    pub async fn wait(&self) {
        self.wake.notified().await
    }

    /// Every subagent on a line each (what TaskStatus shows); `None` when there are none.
    pub fn report(&self, only: Option<&str>) -> Option<String> {
        let list = self.list.lock().unwrap();
        let lines: Vec<String> = list.iter().filter(|a| only.is_none_or(|k| a.id == k || a.name.as_deref() == Some(k))).map(|a| a.summary()).collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// A reminder about background subagents that have gone quiet. Each is reported once, until
    /// it does something again.
    pub fn stalled_note(&self, p: Prompts) -> Option<Part> {
        let lines: Vec<String> = self
            .list
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.background.load(Ordering::SeqCst) && *a.status.lock().unwrap() == Status::Running)
            .filter_map(|a| a.child.live.take_stall().map(|_| a.summary()))
            .collect();
        if lines.is_empty() {
            return None;
        }
        for l in &lines {
            tracing::warn!("subagent stalled: {l}");
        }
        Some(Part::Text(prompt::reminder(&p.msg("stalled_agents", &[("body", &lines.join("\n"))]))))
    }

    /// Every subagent's state, as the client and `status.json` show it.
    pub fn status(&self) -> Vec<serde_json::Value> {
        self.list.lock().unwrap().iter().map(|a| a.status_json()).collect()
    }

    /// Stop a running subagent, by id or name. Whether there was one to stop.
    pub fn stop(&self, key: &str) -> bool {
        match self.find(key) {
            Some(a) if *a.status.lock().unwrap() == Status::Running => {
                a.cancel.lock().unwrap().cancel();
                true
            }
            _ => false,
        }
    }

    /// Write every subagent's state to `path`, for anyone looking from outside the app.
    pub fn write_status(&self, path: &Path) {
        let agents = self.status();
        if agents.is_empty() {
            return;
        }
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let doc = serde_json::json!({"updated_at_ms": at, "agents": agents});
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, doc.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Stop every running background subagent.
    pub fn stop_background(&self) {
        for a in self.list.lock().unwrap().iter() {
            if a.background.load(Ordering::SeqCst) && *a.status.lock().unwrap() == Status::Running {
                a.cancel.lock().unwrap().cancel();
            }
        }
    }
}

/// The notes as a part for the main agent's next message.
pub fn notes_part(notes: &[Note], p: Prompts) -> Option<Part> {
    if notes.is_empty() {
        return None;
    }
    let body: Vec<String> = notes
        .iter()
        .map(|n| {
            format!(
                "<task-notification>\n<agent-id>{}</agent-id>\n<description>{}</description>\n<status>{}</status>\n<result>\n{}\n</result>\n</task-notification>",
                n.agent_id, n.description, n.status, n.text
            )
        })
        .collect();
    Some(Part::Text(prompt::reminder(&p.msg("notes_intro", &[("body", &body.join("\n\n"))]))))
}

/// Add a message to a subagent's history and to its transcript on disk.
pub async fn record(child: &Session, m: Message) {
    child.store.append(&Entry::Msg { m: m.clone() });
    child.state.lock().await.messages.push(m);
}

/// Run the agent until it answers without asking for tools; returns its report. Messages left in
/// its inbox (and the one that started this run, already in its history) are read as it goes.
pub async fn run(parent: &Arc<Session>, agent: &Arc<SubAgent>, cancel: &CancellationToken) -> Result<String, String> {
    let child = &agent.child;
    let live = &child.live;
    // Replies in a row cut off by the output token limit.
    let mut cut_offs = 0;
    for _ in 0..parent.host.settings().limits.subagent_max_steps.max(1) {
        let inbox: Vec<String> = std::mem::take(&mut *agent.inbox.lock().unwrap());
        if !inbox.is_empty() {
            let msg = Message::with_parts(
                Role::User,
                inbox.into_iter().map(|m| Part::Text(parent.prompts().msg("inbox", &[("text", &m)]))).collect(),
            );
            record(child, msg).await;
        }
        let (model_id, messages) = {
            let st = child.state.lock().await;
            (st.model_id.clone(), st.messages.clone())
        };
        let mc = parent.model_client(&model_id).await?;
        let mut system = mc.system_prefix.clone();
        system.extend(agent.system.clone());
        let request = Request { system, messages, tools: agent.registry.specs(false, child.prompts()), ..Default::default() };
        live.step();
        live.set("waiting for the model");
        let response = match mc.client.complete(&request, cancel, |_| live.touch()).await {
            Ok(r) => r,
            Err(e) if e.cancelled => return Err("stopped".into()),
            Err(e) => return Err(format!("the subagent failed: {e}")),
        };
        if let Some(u) = response.usage {
            let mut t = parent.extra_usage.lock().unwrap();
            t.input += u.input;
            t.cached_read += u.cached_read;
            t.cache_write += u.cache_write;
            t.output += u.output;
            t.reasoning += u.reasoning;
            drop(t);
            parent.add_cost(u.cost_usd);
        }
        let cut = response.stop == patchbay_llm::StopReason::MaxTokens;
        cut_offs = if cut { cut_offs + 1 } else { 0 };
        let calls: Vec<_> = response.message.tool_calls().cloned().collect();
        let cut_off = turn::cut_off_call(&calls, &response.stop);
        let text = response.message.text();
        record(child, response.message).await;
        if calls.is_empty() {
            // Cut off by the output token limit: ask for the rest, a few times at most.
            if cut && cut_offs <= turn::MAX_CUT_OFF_CONTINUES {
                let note = prompt::reminder(&parent.prompts().msg("cut_off_reply", &[]));
                record(child, Message::with_parts(Role::User, vec![Part::Text(note)])).await;
                continue;
            }
            // A message that arrived while it was answering gets an answer too.
            if !agent.inbox.lock().unwrap().is_empty() {
                continue;
            }
            return if text.trim().is_empty() { Err("the subagent finished without a report".into()) } else { Ok(text) };
        }
        live.set(format!("running {}", calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")));
        let results = turn::execute(child, &agent.registry, calls, cut_off, cancel, "↳ ").await;
        live.touch();
        record(child, Message::with_parts(Role::User, results.into_iter().map(Part::ToolResult).collect())).await;
        if cancel.is_cancelled() {
            return Err("stopped".into());
        }
    }
    Err("the subagent used up its step limit before it finished".into())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Check out a worktree for a subagent, from the current commit. Returns it and the directory in
/// it that matches `cwd`.
pub fn create_worktree(cwd: &Path, data_dir: &Path, agent_id: &str) -> Result<(Worktree, PathBuf), String> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).map_err(|_| "worktree isolation needs a git repository".to_owned())?);
    let base = git(&top, &["rev-parse", "HEAD"]).map_err(|_| "worktree isolation needs a commit to start from".to_owned())?;
    let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let branch = format!("patchbay/{agent_id}-{short}");
    let path = data_dir.join("agent").join("worktrees").join(format!("{agent_id}-{short}"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    git(&top, &["worktree", "add", "-q", "-b", &branch, &path.to_string_lossy(), "HEAD"])?;
    let inner = cwd.strip_prefix(&top).map(|r| path.join(r)).unwrap_or_else(|_| path.clone());
    Ok((Worktree { path, branch, repo: top, base }, inner))
}

/// Remove the worktree if the subagent left it unchanged; otherwise keep it and say where it is.
pub fn finish_worktree(w: &Worktree) -> Option<String> {
    let dirty = git(&w.path, &["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(true);
    let moved = git(&w.path, &["rev-parse", "HEAD"]).map(|h| h != w.base).unwrap_or(true);
    if !dirty && !moved {
        let _ = git(&w.repo, &["worktree", "remove", "--force", &w.path.to_string_lossy()]);
        let _ = git(&w.repo, &["branch", "-D", &w.branch]);
        return None;
    }
    Some(format!(
        "It changed files in its own git worktree: {} (branch {}). Review the changes there and merge or cherry-pick what you want; `git worktree remove` deletes it.",
        w.path.display(),
        w.branch
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_become_one_reminder() {
        assert!(notes_part(&[], Prompts::new(false)).is_none());
        let note = || Note { agent_id: "agent_1".into(), description: "scan".into(), status: "completed", text: "found it".into() };
        let p = notes_part(&[note()], Prompts::new(false)).unwrap();
        let Part::Text(t) = p else { panic!() };
        assert!(prompt::is_reminder(&t) && t.contains("<agent-id>agent_1</agent-id>") && t.contains("found it"));
        let Some(Part::Text(min)) = notes_part(&[note()], Prompts::new(true)) else { panic!() };
        assert!(min.len() < t.len() && min.contains("<agent-id>agent_1</agent-id>"));
    }

    /// `l` as if it last did something `ago` back.
    fn quiet_for(l: &Liveness, ago: Duration) {
        l.inner.lock().unwrap().last = Instant::now().checked_sub(ago).expect("the machine has been up a while");
    }

    #[test]
    fn a_quiet_session_is_reported_once_until_it_moves_again() {
        let l = Liveness::default();
        assert!(l.take_stall().is_none(), "a session that just started is not stalled");
        quiet_for(&l, STALL_AFTER - Duration::from_secs(5));
        assert!(l.take_stall().is_none());
        quiet_for(&l, STALL_AFTER + Duration::from_secs(5));
        let p = l.take_stall().expect("quiet too long");
        assert!(!p.blocked && p.idle >= STALL_AFTER);
        assert!(l.take_stall().is_none(), "reported once");
        l.touch();
        quiet_for(&l, STALL_AFTER + Duration::from_secs(5));
        assert!(l.take_stall().is_some(), "moving again re-arms the report");
    }

    #[test]
    fn waiting_for_the_user_is_reported_sooner() {
        let l = Liveness::default();
        l.set("waiting for the model");
        quiet_for(&l, BLOCKED_AFTER + Duration::from_secs(5));
        assert!(l.take_stall().is_none(), "a model call gets the longer allowance");
        l.block("waiting for the user to approve: Bash");
        quiet_for(&l, BLOCKED_AFTER + Duration::from_secs(5));
        let p = l.take_stall().expect("a permission prompt left unanswered");
        assert!(p.blocked && p.current.contains("approve"));
        l.set("running Bash");
        assert!(!l.pulse().blocked, "answering the prompt clears it");
    }

    #[test]
    fn spans_read_as_short_durations() {
        assert_eq!(span(Duration::from_secs(5)), "5s");
        assert_eq!(span(Duration::from_secs(75)), "1m 15s");
        assert_eq!(span(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn an_empty_registry_has_nothing_to_report_or_write() {
        let a = Agents::default();
        assert!(a.report(None).is_none() && a.report(Some("agent_1")).is_none());
        assert!(a.stalled_note(Prompts::new(false)).is_none());
        assert!(!a.stop("agent_1") && a.status().is_empty());
        let path = std::env::temp_dir().join(format!("pb-status-{}", uuid::Uuid::new_v4())).join("status.json");
        a.write_status(&path);
        assert!(!path.exists(), "no agents, no file");
    }

    #[test]
    fn finds_agents_by_id_or_name() {
        // A registry with nothing in it finds nothing, and ids count up.
        let a = Agents::default();
        assert!(a.find("x").is_none());
        assert_eq!((a.next_id().as_str(), a.next_id().as_str()), ("agent_1", "agent_2"));
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn worktrees_isolate_edits_and_clean_up() {
        let base = std::env::temp_dir().join(format!("pb-wt-{}", uuid::Uuid::new_v4()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| git(&repo, args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run(&["add", "."]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);

        let (w, dir) = create_worktree(&repo, &base.join("data"), "agent_1").unwrap();
        assert!(dir.join("a.txt").is_file());
        assert!(finish_worktree(&w).is_none(), "an untouched worktree is removed");
        assert!(!w.path.exists());

        let (w, dir) = create_worktree(&repo, &base.join("data"), "agent_2").unwrap();
        std::fs::write(dir.join("b.txt"), "new").unwrap();
        let note = finish_worktree(&w).expect("kept");
        assert!(note.contains(&w.branch) && w.path.join("b.txt").is_file());
        assert!(!repo.join("b.txt").exists(), "the main checkout is untouched");
        let _ = std::fs::remove_dir_all(base);
    }
}
