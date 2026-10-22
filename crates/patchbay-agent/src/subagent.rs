//! Subagents: other model loops the main agent starts, each in a session of its own.
//!
//! A subagent has its own history, files, shells and (optionally) its own git worktree. It runs
//! in the turn that started it, next to other subagents, or in the background; a background
//! one reports through a notification the main agent reads on its next step.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use patchbay_llm::{Message, Part, Request, Role, SystemBlock};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::prompt;
use crate::session::Session;
use crate::tools::Registry;
use crate::turn;

/// Most model requests one subagent run may make.
const MAX_STEPS: usize = 40;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed(String),
    Stopped,
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
pub fn notes_part(notes: &[Note]) -> Option<Part> {
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
    Some(Part::Text(prompt::reminder(&format!(
        "Background subagents have finished. Their results are below; read them and carry on. Do not repeat them verbatim to the user unless asked.\n\n{}",
        body.join("\n\n")
    ))))
}

/// Run the agent until it answers without asking for tools; returns its report. Messages left in
/// its inbox (and the one that started this run, already in its history) are read as it goes.
pub async fn run(parent: &Arc<Session>, agent: &Arc<SubAgent>, cancel: &CancellationToken) -> Result<String, String> {
    let child = &agent.child;
    for _ in 0..MAX_STEPS {
        let inbox: Vec<String> = std::mem::take(&mut *agent.inbox.lock().unwrap());
        if !inbox.is_empty() {
            child.state.lock().await.messages.push(Message::with_parts(
                Role::User,
                inbox.into_iter().map(|m| Part::Text(format!("Message from the main agent:\n{m}"))).collect(),
            ));
        }
        let (model_id, messages) = {
            let st = child.state.lock().await;
            (st.model_id.clone(), st.messages.clone())
        };
        let mc = parent.host.model_client(&model_id).await?;
        let mut system = mc.system_prefix.clone();
        system.extend(agent.system.clone());
        let request = Request { system, messages, tools: agent.registry.specs(false), ..Default::default() };
        let response = match mc.client.complete(&request, cancel, |_| {}).await {
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
        }
        let calls: Vec<_> = response.message.tool_calls().cloned().collect();
        let text = response.message.text();
        child.state.lock().await.messages.push(response.message);
        if calls.is_empty() {
            // A message that arrived while it was answering gets an answer too.
            if !agent.inbox.lock().unwrap().is_empty() {
                continue;
            }
            return if text.trim().is_empty() { Err("the subagent finished without a report".into()) } else { Ok(text) };
        }
        let results = turn::execute(child, &agent.registry, calls, cancel, "↳ ").await;
        child
            .state
            .lock()
            .await
            .messages
            .push(Message::with_parts(Role::User, results.into_iter().map(Part::ToolResult).collect()));
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
        assert!(notes_part(&[]).is_none());
        let p = notes_part(&[Note { agent_id: "agent_1".into(), description: "scan".into(), status: "completed", text: "found it".into() }]).unwrap();
        let Part::Text(t) = p else { panic!() };
        assert!(prompt::is_reminder(&t) && t.contains("<agent-id>agent_1</agent-id>") && t.contains("found it"));
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
