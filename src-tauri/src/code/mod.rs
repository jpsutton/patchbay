//! Code tab: coding agents speaking ACP: Patchbay's own (in-process) and external ones
//! (Grok Build, Claude Code, OpenCode, Cursor CLI, Kilo Code, …) over stdio.

pub mod acp;
pub mod agent_providers;
pub mod agent_state;
pub mod agents;
pub mod grok_config;
pub mod mcp;
pub mod native;
pub mod permissions;
pub mod plan_limits;
pub mod questions;
pub mod titles;
pub mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use self::acp::{AgentCmd, PendingPermissions, PendingQuestions, Sink, SpawnSpec};
use crate::db::Db;

struct Agent {
    tx: mpsc::UnboundedSender<AgentCmd>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub struct CodeManager {
    agents: Mutex<HashMap<String, Agent>>,
    pub pending: PendingPermissions,
    pub questions: PendingQuestions,
    /// Sessions whose agent is in the middle of a turn.
    busy: Mutex<std::collections::HashSet<String>>,
    /// Sessions being handed to another node: no restarts until the handover settles.
    moving: Mutex<HashMap<String, std::time::Instant>>,
    /// The newest agent started for each session. An agent stopped while still starting keeps
    /// running until it reads its `Shutdown`; anything it sends after a newer one starts is dropped.
    generations: Arc<Mutex<HashMap<String, u64>>>,
    next_generation: std::sync::atomic::AtomicU64,
    pub titles: titles::Titles,
    pub plan: plan_limits::PlanGuard,
}

impl CodeManager {
    pub fn set_busy(&self, code_session_id: &str, busy: bool) {
        let mut b = self.busy.lock().unwrap();
        if busy {
            b.insert(code_session_id.to_owned());
        } else {
            b.remove(code_session_id);
        }
    }

    /// Stand in for a running agent: commands for the session come out of the returned channel.
    #[cfg(test)]
    pub fn attach_test_agent(&self, code_session_id: &str) -> mpsc::UnboundedReceiver<AgentCmd> {
        let (tx, rx) = mpsc::unbounded_channel();
        let thread = std::thread::spawn(|| {});
        self.agents.lock().unwrap().insert(code_session_id.to_owned(), Agent { tx, thread });
        rx
    }

    pub fn busy_sessions(&self) -> Vec<String> {
        self.busy.lock().unwrap().iter().cloned().collect()
    }

    /// Mid-turn, or waiting on a permission answer.
    pub fn is_busy(&self, code_session_id: &str) -> bool {
        self.is_running(code_session_id)
            && (self.busy.lock().unwrap().contains(code_session_id)
                || self.pending.lock().unwrap().values().any(|(s, _)| s == code_session_id))
    }

    pub fn set_moving(&self, code_session_id: &str, moving: bool) {
        let mut m = self.moving.lock().unwrap();
        if moving {
            m.insert(code_session_id.to_owned(), std::time::Instant::now());
        } else {
            m.remove(code_session_id);
        }
    }

    /// A handover that never completed stops blocking after a while.
    pub fn is_moving(&self, code_session_id: &str) -> bool {
        self.moving
            .lock()
            .unwrap()
            .get(code_session_id)
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(15 * 60))
    }

    pub fn is_running(&self, code_session_id: &str) -> bool {
        self.agents
            .lock()
            .unwrap()
            .get(code_session_id)
            .is_some_and(|a| !a.tx.is_closed())
    }

    /// Mark a new agent as this session's newest. The returned check stays true until another starts.
    fn new_generation(&self, code_session_id: &str) -> impl Fn() -> bool + Clone + Send + Sync + 'static {
        let generation = self.next_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.generations
            .lock()
            .unwrap()
            .insert(code_session_id.to_owned(), generation);
        let generations = self.generations.clone();
        let csid = code_session_id.to_owned();
        move || generations.lock().unwrap().get(&csid) == Some(&generation)
    }

    pub fn start(
        &self,
        sink: Sink,
        db: Arc<Db>,
        data_dir: &Path,
        code_session_id: &str,
    ) -> Result<()> {
        // Let a previous agent for this session flush and exit first, so the new one loads
        // its final state and the old "exited" event can't land after the new "ready".
        self.stop_and_wait(code_session_id, std::time::Duration::from_secs(5));
        let session = db
            .get_code_session(code_session_id)?
            .ok_or_else(|| anyhow!("code session not found"))?;
        let launch = agents::prepare(&db, data_dir, &session)?;
        // A new agent process keeps its own usage ledger; carry earlier totals forward.
        let mut usage = db.code_usage(code_session_id)?;
        usage.roll_over();
        db.set_code_usage(code_session_id, &usage)?;
        let usage = Arc::new(Mutex::new(usage));
        let current = self.new_generation(code_session_id);
        let sink: Sink = {
            let db = db.clone();
            let csid = code_session_id.to_owned();
            let current = current.clone();
            Arc::new(move |env: acp::Envelope| {
                if !current() {
                    return;
                }
                let limits = match &env.event {
                    acp::CodeEvent::ContextUsage { update } => plan_limits::PlanLimits::from_update(update),
                    _ => None,
                };
                let view = {
                    let mut u = usage.lock().unwrap();
                    match &env.event {
                        acp::CodeEvent::SessionStatus { status } => u.apply_status(status),
                        acp::CodeEvent::ContextUsage { update } => u.apply_context(update),
                        acp::CodeEvent::TurnUsage { usage } => u.apply_turn(usage),
                        _ => {
                            drop(u);
                            return sink(env);
                        }
                    }
                    let _ = db.set_code_usage(&csid, &u);
                    u.view()
                };
                sink(acp::Envelope {
                    code_session_id: env.code_session_id.clone(),
                    event: acp::CodeEvent::Usage { usage: view },
                });
                if let Some(limits) = limits {
                    sink(acp::Envelope {
                        code_session_id: env.code_session_id,
                        event: acp::CodeEvent::PlanLimits { limits },
                    });
                }
            })
        };
        let csid = code_session_id.to_owned();
        let db2 = db.clone();
        let (tx, thread) = acp::spawn(SpawnSpec {
            sink,
            code_session_id: csid.clone(),
            agent: session.agent.clone(),
            launch,
            cwd: PathBuf::from(&session.cwd),
            resume: session.acp_session_id.clone(),
            pending: self.pending.clone(),
            questions: self.questions.clone(),
            on_session: {
                let csid = csid.clone();
                Box::new(move |sid| {
                    // A replaced agent's session id would make the new agent try to load it.
                    if current() {
                        let _ = db2.update_code_session(&csid, Some(&sid), None, None);
                    }
                })
            },
            auto_approve: {
                let db = db.clone();
                let cwd = session.cwd.clone();
                Arc::new(move |tool_call, options| {
                    // Read fresh each time so rules and the session's Ask/Auto/Always apply at once.
                    let rules = db.list_permission_rules().ok()?;
                    let mode = db
                        .get_code_session(&csid)
                        .ok()
                        .flatten()
                        .map(|s| s.permission_mode)
                        .unwrap_or_else(|| "default".into());
                    permissions::decide(&mode, &rules, &cwd, tool_call, options)
                })
            },
            mcp: {
                let db = db.clone();
                Box::new(move |init| {
                    mcp::acp_servers(&db, mcp::McpSupport::from_init(init)).unwrap_or_else(|e| {
                        tracing::warn!("MCP servers: {e:#}");
                        (Vec::new(), Vec::new())
                    })
                })
            },
        });
        self.agents
            .lock()
            .unwrap()
            .insert(code_session_id.to_owned(), Agent { tx, thread });
        Ok(())
    }

    fn sender(&self, code_session_id: &str) -> Result<mpsc::UnboundedSender<AgentCmd>> {
        self.agents
            .lock()
            .unwrap()
            .get(code_session_id)
            .filter(|a| !a.tx.is_closed())
            .map(|a| a.tx.clone())
            .ok_or_else(|| anyhow!("agent is not running for this session"))
    }

    pub fn send(&self, code_session_id: &str, cmd: AgentCmd) -> Result<()> {
        self.sender(code_session_id)?
            .send(cmd)
            .map_err(|_| anyhow!("agent stopped"))
    }

    /// Send a prompt: a new turn, or mid-turn steered into the running one or queued behind it.
    /// `force` cancels the running turn instead and sends the prompt next.
    pub async fn prompt(&self, code_session_id: &str, msg: acp::UserMessage, force: bool) -> Result<acp::Delivery> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::Prompt { msg, force, reply: Some(reply) })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    pub async fn set_model(&self, code_session_id: &str, model_id: String) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::SetModel { model_id, reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    pub async fn set_mode(&self, code_session_id: &str, mode_id: String) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::SetMode { mode_id, reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    pub async fn set_config_option(
        &self,
        code_session_id: &str,
        config_id: String,
        value: String,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::SetConfigOption { config_id, value, reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    pub async fn rewind(
        &self,
        code_session_id: &str,
        ordinal: usize,
        preview: String,
        revert_files: bool,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(
            code_session_id,
            AgentCmd::Rewind {
                ordinal,
                preview,
                revert_files,
                reply,
            },
        )?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    /// Sign in with one of the agent's advertised methods. Resolves once the session is up.
    pub async fn authenticate(
        &self,
        code_session_id: &str,
        method_id: String,
        api_key: Option<String>,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::Authenticate { method_id, api_key, reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    /// The login command of the agent's terminal sign-in method `method_id`.
    pub async fn terminal_auth(&self, code_session_id: &str, method_id: String) -> Result<crate::terminal::Program> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::TerminalAuth { method_id, reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    pub fn respond_permission(&self, request_id: &str, option_id: Option<String>) {
        if let Some((_, tx)) = self.pending.lock().unwrap().remove(request_id) {
            let _ = tx.send(option_id);
        }
    }

    /// Answer a question card. A reply that isn't the card's shape is ignored, so the card stays.
    pub fn respond_question(&self, request_id: &str, answers: serde_json::Value) {
        let Ok(reply) = serde_json::from_value::<questions::QuestionReply>(serde_json::json!({ "answers": answers })) else {
            return;
        };
        if let Some((_, tx)) = self.questions.lock().unwrap().remove(request_id) {
            let _ = tx.send(reply);
        }
    }

    pub fn stop(&self, code_session_id: &str) {
        if let Some(a) = self.agents.lock().unwrap().remove(code_session_id) {
            let _ = a.tx.send(AgentCmd::Shutdown);
        }
    }

    /// Blocking: stop the agent and wait (up to `timeout`) for its thread to finish.
    pub fn stop_and_wait(&self, code_session_id: &str, timeout: std::time::Duration) {
        let Some(a) = self.agents.lock().unwrap().remove(code_session_id) else {
            return;
        };
        let _ = a.tx.send(AgentCmd::Shutdown);
        let t0 = std::time::Instant::now();
        while !a.thread.is_finished() && t0.elapsed() < timeout {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    pub fn stop_all(&self) {
        let agents: Vec<Agent> = self.agents.lock().unwrap().drain().map(|(_, a)| a).collect();
        for a in &agents {
            let _ = a.tx.send(AgentCmd::Shutdown);
        }
        // On app exit, give agents a moment to flush their session files.
        let t0 = std::time::Instant::now();
        while agents.iter().any(|a| !a.thread.is_finished())
            && t0.elapsed() < std::time::Duration::from_secs(3)
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CodeManager;

    #[test]
    fn replaced_agent_goes_quiet() {
        let m = CodeManager::default();
        let cursor = m.new_generation("s1");
        let other_session = m.new_generation("s2");
        assert!(cursor());
        // Switching agents starts a new one while the old one may still be starting up.
        let claude = m.new_generation("s1");
        assert!(!cursor());
        assert!(claude());
        assert!(other_session());
    }
}
