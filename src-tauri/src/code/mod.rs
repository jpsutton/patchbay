//! Code tab: coding agents speaking ACP: Patchbay's own (in-process) and external ones
//! (Grok Build, Claude Code, OpenCode, Cursor CLI, Kilo Code, …) over stdio.

pub mod acp;
pub mod agent_providers;
pub mod agent_state;
pub mod agents;
pub mod grok_config;
pub mod import_claude;
pub mod mcp;
pub mod native;
pub mod opencode_state;
pub mod antigravity_plan;
pub mod cancel_note;
pub mod cursor_plan;
pub mod grok_plan;
pub mod permissions;
pub mod plan_limits;
pub mod questions;
pub mod stall;
pub mod titles;
pub mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use self::acp::{
    AgentCmd, PendingPermission, PendingPermissions, PendingQuestions, Sink, SpawnSpec,
};
use crate::db::Db;

struct Agent {
    tx: mpsc::UnboundedSender<AgentCmd>,
    stop_requested: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    stopping: bool,
    /// U1: this process was spawned while filling a worker seat. Lives with the agent handle;
    /// retained until stop is confirmed, including after an unverified stop failure. Independent of teams.db.
    launched_as_worker: bool,
    process_group: Arc<AtomicU32>,
    group_stop_failed: Arc<AtomicBool>,
}

fn request_agent_shutdown(agent: &Agent) {
    // A command whose ready-loop handoff already passed its stop check is in flight; process-group
    // termination ends it like a user turn that was already running when Stop was pressed.
    agent.stop_requested.store(true, Ordering::SeqCst);
    let _ = agent.tx.send(AgentCmd::Shutdown);
}

#[derive(Clone)]
struct LaunchGeneration {
    session_id: String,
    generation: u64,
    generations: Arc<Mutex<HashMap<String, GenerationState>>>,
}

struct GenerationState {
    generation: u64,
    owner_session_id: Option<String>,
}

impl LaunchGeneration {
    fn is_current(&self) -> bool {
        self.generations.lock().unwrap().get(&self.session_id).is_some_and(|s| s.generation == self.generation)
    }
}

/// A session's last turn outcome as the event loop saw it (pb-20261110-04). Affirmative
/// evidence only: `Clean` on a prompt that finished without an error, `Error` when the agent
/// reported one. Nothing recorded proves nothing, so the state maps to `unknown`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Clean,
    Error(String),
}

#[derive(Default)]
pub struct CodeManager {
    agents: Mutex<HashMap<String, Agent>>,
    pub pending: PendingPermissions,
    /// Why a turn with a card open was cancelled, until the agent and the senders are told.
    pub cancel_notes: cancel_note::CancelNotes,
    /// Event sinks retained so coordinator-initiated redispatch can publish a local confirmation.
    event_sinks: Mutex<HashMap<String, Sink>>,
    process_source: Mutex<Option<Arc<dyn crate::teams::process_sweep::ProcessSource>>>,
    local_confirmations_resolved: Mutex<std::collections::HashSet<String>>,
    pub questions: PendingQuestions,
    /// Sessions whose agent is in the middle of a turn.
    busy: Mutex<std::collections::HashSet<String>>,
    /// The last turn's outcome per session (`None` when no turn ended since the manager started).
    turn_ends: Mutex<HashMap<String, TurnEnd>>,
    /// Sessions being handed to another node: no restarts until the handover settles.
    moving: Mutex<HashMap<String, std::time::Instant>>,
    /// The newest agent started for each session. An agent stopped while still starting keeps
    /// running until it reads its `Shutdown`; anything it sends after a newer one starts is dropped.
    generations: Arc<Mutex<HashMap<String, GenerationState>>>,
    next_generation: std::sync::atomic::AtomicU64,
    pub titles: titles::Titles,
    pub plan: plan_limits::PlanGuard,
    pub cursor_plan: cursor_plan::CursorPlanGuard,
    pub antigravity_plan: antigravity_plan::AntigravityPlanGuard,
    /// Turns the agent has gone quiet in (`stall`).
    pub stall: stall::StallWatch,
    /// Tests: `start` attaches a stand-in agent and hands its commands out here instead of spawning one.
    #[cfg(test)]
    pub test_started: Mutex<Option<mpsc::UnboundedSender<(String, mpsc::UnboundedReceiver<AgentCmd>)>>>,
    #[cfg(test)]
    pub before_second_gate: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
        self.agents.lock().unwrap().insert(
            code_session_id.to_owned(),
            Agent {
                tx,
                stop_requested: Arc::new(AtomicBool::new(false)),
                thread,
                stopping: false,
                launched_as_worker: false,
                process_group: Arc::new(AtomicU32::new(0)),
                group_stop_failed: Arc::new(AtomicBool::new(false)),
            },
        );
        rx
    }

    /// U1: running agent was launched as a worker (immutable for that process).
    pub fn process_launched_as_worker(&self, code_session_id: &str) -> bool {
        self.agents
            .lock()
            .unwrap()
            .get(code_session_id)
            .is_some_and(|a| a.launched_as_worker && !a.tx.is_closed())
    }

    #[cfg(test)]
    pub(crate) fn set_process_source_for_test(&self, source: Arc<dyn crate::teams::process_sweep::ProcessSource>) {
        *self.process_source.lock().unwrap() = Some(source);
    }

    pub fn busy_sessions(&self) -> Vec<String> {
        self.busy.lock().unwrap().iter().cloned().collect()
    }

    /// Record how a turn ended, so `done` and `stuck` rest on evidence rather than on the
    /// agent's absence (`session_status`).
    pub fn note_turn_end(&self, code_session_id: &str, end: TurnEnd) {
        self.turn_ends.lock().unwrap().insert(code_session_id.to_owned(), end);
    }

    /// The last turn's outcome, `None` when no turn has ended since the manager started.
    pub fn turn_end(&self, code_session_id: &str) -> Option<TurnEnd> {
        self.turn_ends.lock().unwrap().get(code_session_id).cloned()
    }

    /// A new turn is starting: the previous outcome no longer describes this session, so a
    /// fixed error stops reading as `stuck` (and a fresh error will be recorded again).
    pub fn clear_turn_end(&self, code_session_id: &str) {
        self.turn_ends.lock().unwrap().remove(code_session_id);
    }

    /// Mid-turn, or waiting on a permission answer.
    pub fn is_busy(&self, code_session_id: &str) -> bool {
        self.is_running(code_session_id)
            && (self.busy.lock().unwrap().contains(code_session_id)
                || self.pending.lock().unwrap().values().any(|p| p.session_id == code_session_id))
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
            .is_some_and(|a| !a.stopping && !a.tx.is_closed())
    }

    /// Mark a new agent as this session's newest. The returned check stays true until another starts.
    fn new_generation(&self, code_session_id: &str) -> LaunchGeneration {
        let generation = self.next_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.generations
            .lock()
            .unwrap()
            .insert(code_session_id.to_owned(), GenerationState { generation, owner_session_id: None });
        LaunchGeneration {
            session_id: code_session_id.to_owned(),
            generation,
            generations: self.generations.clone(),
        }
    }

    fn invalidate_generation(&self, code_session_id: &str) {
        let mut generations = self.generations.lock().unwrap();
        generations.remove(code_session_id);
        generations.retain(|_, state| state.owner_session_id.as_deref() != Some(code_session_id));
    }

    fn require_current_generation(&self, generation: &LaunchGeneration) -> Result<()> {
        if generation.is_current() {
            Ok(())
        } else {
            anyhow::bail!("worker start cancelled before launch")
        }
    }

    fn lock_current_generation<'a>(
        &'a self,
        generation: &LaunchGeneration,
    ) -> Result<std::sync::MutexGuard<'a, HashMap<String, GenerationState>>> {
        let guard = self.generations.lock().unwrap();
        if guard.get(&generation.session_id).is_some_and(|s| s.generation == generation.generation) {
            Ok(guard)
        } else {
            anyhow::bail!("worker start cancelled before launch")
        }
    }

    fn with_current_generation<T>(
        &self,
        generation: &LaunchGeneration,
        action: impl FnOnce() -> T,
    ) -> Result<T> {
        let _generation_guard = self.lock_current_generation(generation)?;
        Ok(action())
    }

    fn with_spawn_generation<T>(
        &self,
        generation: &LaunchGeneration,
        action: impl FnOnce() -> T,
    ) -> Result<T> {
        let mut states = self.lock_current_generation(generation)?;
        let result = action();
        if let Some(state) = states.get_mut(&generation.session_id) {
            state.owner_session_id = None;
        }
        Ok(result)
    }

    fn bind_generation_owner(&self, generation: &LaunchGeneration, owner: Option<&str>) -> Result<()> {
        let mut states = self.lock_current_generation(generation)?;
        states.get_mut(&generation.session_id).expect("checked current generation").owner_session_id = owner.map(str::to_owned);
        Ok(())
    }

    pub fn start(
        &self,
        sink: Sink,
        db: Arc<Db>,
        data_dir: &Path,
        code_session_id: &str,
        message_bridge: Option<crate::agent_messaging::AgentMessagingBridge>,
    ) -> Result<()> {
        let (session, current) = self.preflight_start_with_generation(sink.clone(), &db, data_dir, code_session_id)?;
        self.require_current_generation(&current)?;
        #[cfg(test)]
        {
            // Test stand-ins intentionally bypass backend preparation (which can fail when, for
            // example, the fixture has no model configured). They still traverse the final gate.
            if let Some(tx) = self.test_started.lock().unwrap().as_ref() {
                if let Some(hook) = self.before_second_gate.lock().unwrap().take() { hook(); }
                crate::teams::gate_session_start(&db, data_dir, self, &session)?;
                self.with_current_generation(&current, || {
                    tx.send((code_session_id.to_owned(), self.attach_test_agent(code_session_id)))
                        .map_err(|_| anyhow!("stand-in agent failed to start"))
                })??;
                return Ok(());
            }
        }
        let launch = agents::prepare(&db, data_dir, &session)?;
        #[cfg(test)]
        if let Some(hook) = self.before_second_gate.lock().unwrap().take() { hook(); }
        crate::teams::gate_session_start(&db, data_dir, self, &session)?;
        let process_launched_as_worker = launch.launched_as_worker;
        let process_group = Arc::new(AtomicU32::new(0));
        let group_stop_failed = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        // A new agent process keeps its own usage ledger; carry earlier totals forward.
        let mut usage = db.code_usage(code_session_id)?;
        usage.roll_over();
        db.set_code_usage(code_session_id, &usage)?;
        let usage = Arc::new(Mutex::new(usage));
        let sink: Sink = {
            let db = db.clone();
            let csid = code_session_id.to_owned();
            let current = current.clone();
            Arc::new(move |env: acp::Envelope| {
                if !current.is_current() {
                    return;
                }
                if let acp::CodeEvent::SessionReady { loaded, .. } = &env.event {
                    import_claude::settle_fallback(&db, &csid, *loaded);
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
        let interject = launch.ext.interject;
        let session_generation = current.clone();
        // The second gate and all launch preparation are complete. A Stop/archive during the
        // confirmation or preparation invalidates this generation and must prevent ACP spawn.
        let spec = SpawnSpec {
            sink,
            code_session_id: csid.clone(),
            agent: session.agent.clone(),
            launch,
            cwd: PathBuf::from(&session.cwd),
            data_dir: data_dir.to_path_buf(),
            resume: session.acp_session_id.clone(),
            pending: self.pending.clone(),
            questions: self.questions.clone(),
            on_session: {
                let csid = csid.clone();
                Box::new(move |sid| {
                    // A replaced agent's session id would make the new agent try to load it.
                    if session_generation.is_current() {
                        let _ = db2.update_code_session(&csid, Some(&sid), None, None);
                    }
                })
            },
            permission_mode: {
                let db = db.clone();
                let csid = code_session_id.to_owned();
                Arc::new(move || db.get_code_session(&csid).ok().flatten().map(|s| s.permission_mode))
            },
            auto_approve: {
                let db = db.clone();
                let cwd = session.cwd.clone();
                let data_dir = data_dir.to_path_buf();
                let csid = csid.clone();
                // U1: immutable for this ACP connection — captured at spawn, not re-read from DB.
                Arc::new(move |tool_call, options| {
                    decide_session_auto_approve(
                        &data_dir,
                        &db,
                        &csid,
                        &cwd,
                        tool_call,
                        options,
                        process_launched_as_worker,
                    )
                })
            },
            mcp: {
                let db = db.clone();
                let session_id = code_session_id.to_owned();
                let host_agent = session.agent.clone();
                let data_dir = data_dir.to_path_buf();
                let worker_native = process_launched_as_worker && host_agent == "patchbay";
                Box::new(move |init| {
                    let (mut servers, skipped) = if worker_native { (Vec::new(), Vec::new()) } else {
                        mcp::acp_servers(&db, mcp::McpSupport::from_init(init)).unwrap_or_else(|e| {
                            tracing::warn!("MCP servers: {e:#}");
                            (Vec::new(), Vec::new())
                        })
                    };
                    if !skipped.is_empty() {
                        tracing::warn!("MCP servers skipped for unsupported transports: {}", skipped.join(", "));
                    }
                    // Built after initialize so the tool list can match this agent's steering.
                    if let Some(bridge) = message_bridge {
                        let can_steer = init["_meta"]["steering"]["supported"] == true || interject;
                        // Nonce MCP name only for active team seats; everyone else keeps the fixed name.
                        let team_seat = crate::teams::is_active_team_member(&data_dir, &session_id);
                        match bridge.server_definition(&session_id, &host_agent, can_steer, team_seat) {
                            Ok(server) => servers.insert(0, server),
                            Err(e) => tracing::warn!("agent-messaging MCP: {e:#}"),
                        }
                    }
                    (servers, Vec::new())
                })
            },
            process_group: process_group.clone(),
            group_stop_failed: group_stop_failed.clone(),
            stop_requested: stop_requested.clone(),
        };
        self.with_spawn_generation(&current, || {
            let (tx, thread) = acp::spawn(spec);
            self.agents.lock().unwrap().insert(
                code_session_id.to_owned(),
                Agent {
                    tx,
                    stop_requested,
                    thread,
                    stopping: false,
                    launched_as_worker: process_launched_as_worker,
                    process_group,
                    group_stop_failed,
                },
            );
        })?;
        Ok(())
    }

    /// Stop, sweep and confirm a reused worker worktree, then run the first spawn gate. Shared by
    /// production start and route-level test starts so tests exercise the same route behavior.
    pub(crate) fn preflight_start(
        &self,
        sink: Sink,
        db: &Db,
        data_dir: &Path,
        code_session_id: &str,
    ) -> Result<crate::db::CodeSession> {
        self.preflight_start_with_generation(sink, db, data_dir, code_session_id)
            .map(|(session, _)| session)
    }

    fn preflight_start_with_generation(
        &self,
        sink: Sink,
        db: &Db,
        data_dir: &Path,
        code_session_id: &str,
    ) -> Result<(crate::db::CodeSession, LaunchGeneration)> {
        let current = self.new_generation(code_session_id);
        let known_worker = crate::teams::allow::daemon_knows_worker_session(code_session_id)
            || self.process_launched_as_worker(code_session_id);
        let (stopped, worker_pgid) = self.stop_and_wait_with_group(
            code_session_id,
            std::time::Duration::from_millis(acp::AGENT_MANAGER_STOP_MS),
        );
        if !stopped {
            let pid = worker_pgid.map(|pid| pid.to_string()).unwrap_or_else(|| "unknown".into());
            if known_worker {
                anyhow::bail!("worker could not be stopped (pid {pid}); restart Patchbay before trying again");
            }
            anyhow::bail!("agent could not be stopped (pid {pid}); restart Patchbay before trying again");
        }
        if known_worker && !data_dir.join("teams.db").is_file() {
            // A missing team DB is a direct refusal for a known worker, not a confirmation that
            // could only end in the same refusal after a human waits on it.
            let session = db
                .get_code_session(code_session_id)?
                .ok_or_else(|| anyhow!("code session not found"))?;
            crate::teams::gate_session_start(db, data_dir, self, &session)?;
        }
        let session = db
            .get_code_session(code_session_id)?
            .ok_or_else(|| anyhow!("code session not found"))?;
        self.event_sinks
            .lock()
            .unwrap()
            .insert(code_session_id.to_owned(), sink.clone());
        self.require_current_generation(&current)?;
        if let Some(context) =
            crate::teams::worker_reuse_context(data_dir, db, self, code_session_id)?
        {
            self.bind_generation_owner(&current, context.coordinator_session.as_deref())?;
            #[cfg(target_os = "windows")]
            anyhow::bail!("cannot restart or redispatch this worker in place on Windows because process sweeping is unsupported; create a fresh worker worktree");
            #[cfg(not(target_os = "windows"))]
            {
                let source = self.process_source.lock().unwrap().clone();
                let run_sweep = || -> Result<crate::teams::process_sweep::SweepReport> {
                    if let Some(source) = &source {
                        crate::teams::process_sweep::sweep_with(Path::new(&session.cwd), std::process::id(), worker_pgid, source.as_ref())
                    } else {
                        crate::teams::sweep_worker_worktree(Path::new(&session.cwd), worker_pgid)
                    }
                };
                let note = if context.history_unknown {
                    "worker history could not be verified; treating this as a reused worktree"
                } else {
                    "same-uid process sweep completed"
                };
                if !self.confirm_reused_worker_with_sweep(
                    code_session_id,
                    &sink,
                    context.coordinator_session.as_deref(),
                    &current,
                    note,
                    run_sweep,
                )? {
                anyhow::bail!("worker restart cancelled by the local human");
                }
            }
        }
        self.require_current_generation(&current)?;
        crate::teams::gate_session_start(db, data_dir, self, &session)?;
        Ok((session, current))
    }

    fn sender(&self, code_session_id: &str) -> Result<mpsc::UnboundedSender<AgentCmd>> {
        self.agents
            .lock()
            .unwrap()
            .get(code_session_id)
            .filter(|a| !a.stopping && !a.tx.is_closed())
            .map(|a| a.tx.clone())
            .ok_or_else(|| anyhow!("agent is not running for this session"))
    }

    pub fn send(&self, code_session_id: &str, mut cmd: AgentCmd) -> Result<()> {
        // Every cancel with a card open is explained to the agent's next prompt and to the senders
        // (pb-20261121-01): the agent CLI would otherwise report it as the user rejecting the call.
        match &mut cmd {
            AgentCmd::Cancel => self.note_cancel(code_session_id, "you pressed Stop", "user_stop", None, false),
            AgentCmd::Prompt { msg, force, .. } => {
                if *force {
                    let why = "the user sent a new message with Send now";
                    self.note_cancel(code_session_id, why, "send_now", None, false);
                }
                if let Some(note) = self.cancel_notes.take_for_prompt(code_session_id) {
                    cancel_note::append_reminder(&mut msg.blocks, &note.line());
                }
            }
            _ => {}
        }
        self.send_raw(code_session_id, cmd)
    }

    fn send_raw(&self, code_session_id: &str, cmd: AgentCmd) -> Result<()> {
        self.sender(code_session_id)?
            .send(cmd)
            .map_err(|_| anyhow!("agent stopped"))
    }

    /// Whether a tool-approval card is open for the session: cancelling now answers it "cancelled".
    pub fn has_open_card(&self, code_session_id: &str) -> bool {
        self.pending.lock().unwrap().values().any(|p| {
            !p.local_human_only && (p.session_id == code_session_id || p.target_session_id.as_deref() == Some(code_session_id))
        })
    }

    /// Leave the reason for a cancel that is about to happen. The agent is told only when a card is
    /// open (else no tool call was cancelled); the senders when one is open or `always_senders`.
    fn note_cancel(&self, code_session_id: &str, reason: &str, kind: &'static str, resets_at: Option<u64>, always_senders: bool) {
        let card = self.has_open_card(code_session_id);
        if card || always_senders {
            let note = cancel_note::Note::new(reason, kind, resets_at);
            self.cancel_notes.record(code_session_id, note, card, true);
        }
    }

    /// The plan-limit guard stops a running turn: `reason` says what ran out and when it resets.
    pub fn cancel_for_limit(&self, code_session_id: &str, reason: &str, resets_at: Option<u64>) -> Result<()> {
        self.note_cancel(code_session_id, reason, "usage_limit_stop", resets_at, true);
        self.send_raw(code_session_id, AgentCmd::Cancel)
    }

    /// Send a prompt: a new turn, or mid-turn steered into the running one or queued behind it.
    /// `force` cancels the running turn instead and sends the prompt next.
    pub async fn prompt(&self, code_session_id: &str, msg: acp::UserMessage, force: bool) -> Result<acp::Delivery> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::Prompt { msg, force, reply: Some(reply) })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    /// Peer agent messages waiting behind the running turn.
    pub async fn list_queued_agent_messages(
        &self,
        code_session_id: &str,
    ) -> Result<Vec<acp::QueuedAgentMessage>> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::ListQueuedAgent { reply })?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    /// Take every queued peer agent message out for drop notices (pb-20261107-30): the
    /// caller tells each sender, so a stop/delete/archive/agent-switch never drops one
    /// silently. User prompts in the queue are left alone.
    pub async fn drain_queued_agent_messages(&self, code_session_id: &str) -> Result<Vec<acp::QueuedAgentMessage>> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::DrainQueuedAgent { reply })?;
        Ok(rx.await.map_err(|_| anyhow!("agent stopped"))?)
    }

    /// Queue a peer-message drain NOW and hand back its reply: the loop's channel is FIFO,
    /// so a synchronous stop sent right after cannot overtake it (pb-20261107-30).
    /// `None` when no loop is running — there is no queue to drain.
    pub fn begin_drain_queued_agent(
        &self,
        code_session_id: &str,
    ) -> Option<oneshot::Receiver<Vec<acp::QueuedAgentMessage>>> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::DrainQueuedAgent { reply }).ok()?;
        Some(rx)
    }

    /// The sessions whose agent loop is running — the only ones that can hold a queue.
    pub fn running_sessions(&self) -> Vec<String> {
        self.agents.lock().unwrap().iter()
            .filter(|(_, agent)| !agent.stopping && !agent.tx.is_closed())
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// Take one queued peer agent message out of the queue (MCP accept mid-turn).
    pub async fn accept_queued_agent_message(&self, code_session_id: &str, prompt_id: &str) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.send(
            code_session_id,
            AgentCmd::AcceptQueuedAgent {
                prompt_id: prompt_id.to_owned(),
                reply,
            },
        )?;
        rx.await.map_err(|_| anyhow!("agent stopped"))?
    }

    /// "Send now": a queued prompt goes next (the running turn is cancelled), or one steered
    /// into the running turn is taken out of it. Says what it did, or why it could not, so the
    /// button never does nothing.
    pub async fn send_now(&self, code_session_id: &str, prompt_id: &str) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(
            code_session_id,
            AgentCmd::SendNow {
                prompt_id: prompt_id.to_owned(),
                reply: Some(reply),
            },
        )?;
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

    /// Copy the session into a new agent session (see `AgentCmd::Fork`); returns its id.
    pub async fn fork(&self, code_session_id: &str, ordinal: Option<usize>, preview: String) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.send(code_session_id, AgentCmd::Fork { ordinal, preview, reply })?;
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

    /// Answer a pending permission. An `optionId` not in the offered set is refused with an
    /// error and the request stays pending (nothing reaches the agent). `None` cancels.
    /// When `session_id` is supplied it must equal the pending record's session (G3); mismatch
    /// keeps the request pending. Mesh `code.permission` historically omits session id — leave
    /// that protocol unchanged in #449 (pass `None`).
    pub fn respond_permission(
        &self,
        request_id: &str,
        option_id: Option<String>,
        session_id: Option<&str>,
    ) -> Result<()> {
        let mut map = self.pending.lock().unwrap();
        let Some(entry) = map.get(request_id) else {
            return Ok(());
        };
        if entry.local_human_only && session_id.is_none() {
            return Err(anyhow!(
                "this confirmation can only be answered by the local human"
            ));
        }
        if let Some(sid) = session_id
            && entry.session_id != sid
        {
            return Err(anyhow!("session does not own this permission request"));
        }
        if let Some(id) = option_id.as_deref()
            && !entry.offered.iter().any(|o| o == id)
        {
            return Err(anyhow!("option not offered for this request: {id}"));
        }
        if entry.local_human_only {
            self.local_confirmations_resolved
                .lock()
                .unwrap()
                .insert(request_id.to_owned());
        }
        let acp::PendingPermission { tx, .. } = map.remove(request_id).expect("pending entry");
        let _ = tx.send(option_id);
        Ok(())
    }

    pub(crate) fn is_local_human_confirmation(&self, request_id: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .get(request_id)
            .is_some_and(|p| p.local_human_only)
    }

    pub(crate) fn take_local_confirmation_resolution(&self, request_id: &str) -> bool {
        self.local_confirmations_resolved
            .lock()
            .unwrap()
            .remove(request_id)
    }

    /// Ask the local UI to authorize reuse of a worktree after its process sweep. The synthetic
    /// permission is bound to the chosen local session and rejects sessionless mesh replies.
    pub(crate) fn confirm_reused_worker(
        &self,
        fallback_session: &str,
        fallback_sink: &Sink,
        coordinator_session: Option<&str>,
        found: usize,
        killed: usize,
        remaining: usize,
        unknown: usize,
        complete: bool,
        wall_ms: u128,
        sweep_note: &str,
    ) -> Result<bool> {
        self.confirm_reused_worker_for_generation(
            fallback_session, fallback_sink, coordinator_session, found, killed, remaining,
            unknown, complete, wall_ms, sweep_note, None,
        )
    }

    fn confirm_reused_worker_for_generation(
        &self,
        fallback_session: &str,
        fallback_sink: &Sink,
        coordinator_session: Option<&str>,
        found: usize,
        killed: usize,
        remaining: usize,
        unknown: usize,
        complete: bool,
        wall_ms: u128,
        sweep_note: &str,
        generation: Option<&LaunchGeneration>,
    ) -> Result<bool> {
        let display_session = coordinator_session.unwrap_or(fallback_session);
        let sink = coordinator_session
            .and_then(|id| self.event_sinks.lock().unwrap().get(id).cloned())
            .unwrap_or_else(|| fallback_sink.clone());
        let request_id = crate::db::new_id();
        let (tx, rx) = oneshot::channel();
        let target_session_id = fallback_session.to_owned();
        let request = serde_json::json!({
            "toolCall": {
                "toolCallId": request_id,
                "title": "Confirm worker restart",
                "kind": "other",
                "rawInput": {},
                "_meta": { "patchbay": { "workerRestartConfirm": {
                    "message": format!("a previous worker ran here; {found} processes were found and {killed} killed; {remaining} remain; {unknown} processes could not be checked; sweep {}; {wall_ms} ms; detached processes may still remain.", if complete { "complete" } else { "incomplete" }),
                    "found": found,
                    "killed": killed,
                    "remaining": remaining,
                    "unknown": unknown,
                    "complete": complete,
                    "wallMs": wall_ms,
                    "sweepNote": sweep_note,
                } } }
            },
            "options": [
                { "optionId": "confirm_restart", "name": "Restart worker", "kind": "allow_once" },
                { "optionId": "cancel_restart", "name": "Cancel", "kind": "reject_once" }
            ]
        });
        let request_id = request["toolCall"]["toolCallId"]
            .as_str()
            .unwrap()
            .to_owned();
        let register_and_emit = || {
            self.pending.lock().unwrap().insert(
                request_id.clone(),
                PendingPermission {
                    session_id: display_session.to_owned(),
                    target_session_id: Some(target_session_id),
                    local_human_only: true,
                    offered: vec!["confirm_restart".to_owned(), "cancel_restart".to_owned()],
                    tx,
                },
            );
            sink(acp::Envelope {
                code_session_id: display_session.to_owned(),
                event: acp::CodeEvent::Permission {
                    request_id: request_id.clone(),
                    request,
                },
            });
        };
        if let Some(generation) = generation {
            self.with_current_generation(generation, register_and_emit)?;
        } else {
            register_and_emit();
        }
        let reply = rx
            .blocking_recv()
            .map_err(|_| anyhow!("local human confirmation was dropped"))?;
        self.pending.lock().unwrap().remove(&request_id);
        sink(acp::Envelope {
            code_session_id: display_session.to_owned(),
            event: acp::CodeEvent::PermissionResolved { request_id },
        });
        Ok(reply.as_deref() == Some("confirm_restart"))
    }

    fn confirm_reused_worker_with_sweep<F>(
        &self,
        target_session: &str,
        fallback_sink: &Sink,
        coordinator_session: Option<&str>,
        current: &LaunchGeneration,
        sweep_note: &str,
        mut sweep: F,
    ) -> Result<bool>
    where
        F: FnMut() -> Result<crate::teams::process_sweep::SweepReport>,
    {
        let initial = sweep()?;
        refuse_remaining_processes(&initial)?;
        self.require_current_generation(current)?;
        let (found, killed, remaining) = initial.counts();
        if !self.confirm_reused_worker_for_generation(
            target_session,
            fallback_sink,
            coordinator_session,
            found,
            killed,
            remaining,
            initial.unknown,
            initial.complete,
            initial.wall_ms,
            sweep_note,
            Some(current),
        )? {
            return Ok(false);
        }
        self.require_current_generation(current)?;
        let after = sweep()?;
        refuse_resweep_regression(&initial, &after)?;
        self.require_current_generation(current)?;
        Ok(true)
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
        self.invalidate_generation(code_session_id);
        self.cancel_reuse_confirmations(code_session_id);
        self.note_cancel(code_session_id, "the session was stopped", "shutdown", None, false);
        if let Some(a) = self.agents.lock().unwrap().get_mut(code_session_id) {
            a.stopping = true;
            request_agent_shutdown(a);
        }
    }

    /// Blocking: stop the agent and wait (up to `timeout`) for its thread to finish.
    pub fn stop_and_wait(&self, code_session_id: &str, timeout: std::time::Duration) -> bool {
        self.invalidate_generation(code_session_id);
        self.stop_and_wait_with_group(code_session_id, timeout).0
    }

    fn stop_and_wait_with_group(&self, code_session_id: &str, timeout: std::time::Duration) -> (bool, Option<u32>) {
        self.cancel_reuse_confirmations(code_session_id);
        self.note_cancel(code_session_id, "the session was stopped", "shutdown", None, false);
        let process_group = {
            let mut agents = self.agents.lock().unwrap();
            let Some(agent) = agents.get_mut(code_session_id) else {
                self.event_sinks.lock().unwrap().remove(code_session_id);
                return (true, None);
            };
            agent.stopping = true;
            request_agent_shutdown(agent);
            agent.process_group.load(Ordering::SeqCst)
        };
        let t0 = std::time::Instant::now();
        loop {
            let finished = self.agents.lock().unwrap().get(code_session_id)
                .is_none_or(|agent| agent.thread.is_finished());
            if finished { break; }
            if t0.elapsed() >= timeout { return (false, (process_group > 1).then_some(process_group)); }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let mut agents = self.agents.lock().unwrap();
        let Some(agent) = agents.get(code_session_id) else { return (true, None) };
        if agent.group_stop_failed.load(Ordering::SeqCst) {
            // Keep the failed handle so retries cannot mistake an empty map for a confirmed stop.
            return (false, (process_group > 1).then_some(process_group));
        }
        let a = agents.remove(code_session_id).expect("agent was checked under the same lock");
        drop(agents);
        let _ = a.thread.join();
        self.event_sinks.lock().unwrap().remove(code_session_id);
        (true, (process_group > 1).then_some(process_group))
    }

    fn cancel_reuse_confirmations(&self, session_id: &str) {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<_> = pending.iter()
            .filter(|(_, entry)| entry.local_human_only && (entry.session_id == session_id || entry.target_session_id.as_deref() == Some(session_id)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(entry) = pending.remove(&id) { let _ = entry.tx.send(None); }
        }
    }

    fn cancel_all_reuse_confirmations(&self) {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<_> = pending.iter().filter(|(_, entry)| entry.local_human_only).map(|(id, _)| id.clone()).collect();
        for id in ids {
            if let Some(entry) = pending.remove(&id) { let _ = entry.tx.send(None); }
        }
    }

    pub fn stop_all(&self) {
        self.generations.lock().unwrap().clear();
        self.cancel_all_reuse_confirmations();
        let agents: Vec<Agent> = self.agents.lock().unwrap().drain().map(|(_, a)| a).collect();
        for a in &agents {
            request_agent_shutdown(a);
        }
        let pgids: Vec<u32> = agents.iter().map(|a| a.process_group.load(Ordering::SeqCst)).filter(|pid| *pid > 1).collect();
        // Stop every external group concurrently; the one deadline does not grow with N.
        let group_thread = if pgids.is_empty() { None } else {
            Some(std::thread::spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => {
                        for (pid, outcome) in runtime.block_on(acp::terminate_agent_groups(pgids)) {
                            if let Err(error) = outcome { tracing::error!(pid, "could not stop agent process group during daemon shutdown: {error:#}"); }
                        }
                    }
                    Err(error) => tracing::error!("could not create shutdown process-group runtime: {error:#}"),
                }
            }))
        };
        let t0 = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(acp::AGENT_SHUTDOWN_ALL_MS);
        while (agents.iter().any(|a| !a.thread.is_finished()) || group_thread.as_ref().is_some_and(|thread| !thread.is_finished()))
            && t0.elapsed() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        for a in &agents {
            if !a.thread.is_finished() {
                let pid = a.process_group.load(Ordering::SeqCst);
                tracing::error!(pid, "agent did not stop before daemon shutdown deadline");
            }
        }
        if group_thread.as_ref().is_some_and(|thread| !thread.is_finished()) {
            tracing::error!("agent process-group shutdown did not finish before its overall deadline");
        }
        for a in agents { if a.thread.is_finished() { let _ = a.thread.join(); } }
        if let Some(thread) = group_thread.filter(|thread| thread.is_finished()) { let _ = thread.join(); }
        self.event_sinks.lock().unwrap().clear();
    }
}

fn refuse_remaining_processes(report: &crate::teams::process_sweep::SweepReport) -> Result<()> {
    if let Some(process) = report.remaining.first() {
        anyhow::bail!(
            "worker worktree still has live process {} ({}); stop it and retry",
            process.pid,
            process.command
        );
    }
    Ok(())
}

fn refuse_resweep_regression(
    initial: &crate::teams::process_sweep::SweepReport,
    after: &crate::teams::process_sweep::SweepReport,
) -> Result<()> {
    refuse_remaining_processes(after)?;
    if after.unknown > initial.unknown {
        anyhow::bail!(
            "worker worktree resweep found more unchecked processes ({} versus {}); retry after resolving the scan gap",
            after.unknown,
            initial.unknown
        );
    }
    Ok(())
}

/// Shared auto-approve closure body (G1/U1). Worker-git floor returns ask (None) before team-tool
/// allow, saved permission rules, and mode policy — no rule or Always mode can approve it.
/// `process_launched_as_worker` is the spawn-time capture for this ACP connection (U1).
pub fn decide_session_auto_approve(
    data_dir: &Path,
    db: &Db,
    code_session_id: &str,
    session_cwd: &str,
    tool_call: &serde_json::Value,
    options: &serde_json::Value,
    process_launched_as_worker: bool,
) -> Option<(String, String)> {
    let agent = db
        .get_code_session(code_session_id)
        .ok()
        .flatten()
        .map(|s| s.agent)
        .unwrap_or_default();
    if crate::teams::worker_git_floor_must_ask(
        data_dir,
        code_session_id,
        tool_call,
        process_launched_as_worker,
        &agent,
    ) {
        return None;
    }
    if !agent.is_empty()
        && let Some(hit) =
            crate::teams::allow_team_tool(data_dir, code_session_id, &agent, tool_call, options, session_cwd)
    {
        return Some(hit);
    }
    if process_launched_as_worker
        && agent == "patchbay"
        && permissions::native_worker_must_ask(tool_call, options, session_cwd)
    {
        return None;
    }
    let rules = db.list_permission_rules().ok()?;
    let mode = db
        .get_code_session(code_session_id)
        .ok()
        .flatten()
        .map(|s| s.permission_mode)
        .unwrap_or_else(|| "default".into());
    permissions::decide(&mode, &rules, session_cwd, tool_call, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::acp::PendingPermission;

    #[test]
    fn replaced_agent_goes_quiet() {
        let m = CodeManager::default();
        let cursor = m.new_generation("s1");
        let other_session = m.new_generation("s2");
        assert!(m.require_current_generation(&cursor).is_ok());
        // Switching agents starts a new one while the old one may still be starting up.
        let claude = m.new_generation("s1");
        assert!(m.require_current_generation(&cursor).is_err());
        assert!(m.require_current_generation(&claude).is_ok());
        assert!(m.require_current_generation(&other_session).is_ok());
    }

    #[test]
    fn timed_out_stop_keeps_agent_registered_until_it_exits() {
        let manager = CodeManager::default();
        let (agent_tx, _agent_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || { let _ = release_rx.recv(); });
        manager.agents.lock().unwrap().insert("slow-agent".into(), Agent {
            tx: agent_tx,
            stop_requested: Arc::new(AtomicBool::new(false)),
            thread,
            stopping: false,
            launched_as_worker: false,
            process_group: Arc::new(AtomicU32::new(0)),
            group_stop_failed: Arc::new(AtomicBool::new(false)),
        });

        assert!(!manager.stop_and_wait_with_group("slow-agent", std::time::Duration::from_millis(20)).0);
        assert!(manager.agents.lock().unwrap().contains_key("slow-agent"));
        assert!(!manager.stop_and_wait_with_group("slow-agent", std::time::Duration::from_millis(20)).0);
        assert!(manager.agents.lock().unwrap().contains_key("slow-agent"), "retry must still find the old agent");

        release_tx.send(()).unwrap();
        assert!(manager.stop_and_wait_with_group("slow-agent", std::time::Duration::from_secs(1)).0);
        assert!(!manager.agents.lock().unwrap().contains_key("slow-agent"));
    }

    #[test]
    fn unverified_group_stop_stays_poisoned_across_retries() {
        let manager = CodeManager::default();
        let dir = crate::test_support::TestDir::new("pb-code", "unverified-stop");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let (agent_tx, _agent_rx) = mpsc::unbounded_channel();
        let thread = std::thread::spawn(|| {});
        manager.agents.lock().unwrap().insert("uncertain-agent".into(), Agent {
            tx: agent_tx,
            stop_requested: Arc::new(AtomicBool::new(false)),
            thread,
            stopping: false,
            launched_as_worker: true,
            process_group: Arc::new(AtomicU32::new(456)),
            group_stop_failed: Arc::new(AtomicBool::new(true)),
        });
        let sink: Sink = Arc::new(|_| {});
        let first = manager.preflight_start_with_generation(sink.clone(), &db, &dir, "uncertain-agent").err().unwrap();
        let expected = "worker could not be stopped (pid 456); restart Patchbay before trying again";
        assert!(first.to_string().contains(expected), "{first:#}");
        assert!(manager.agents.lock().unwrap().contains_key("uncertain-agent"));
        let second = manager.preflight_start_with_generation(sink, &db, &dir, "uncertain-agent").err().unwrap();
        assert!(second.to_string().contains(expected), "retry must surface the same recovery instruction: {second:#}");
        assert!(manager.agents.lock().unwrap().contains_key("uncertain-agent"));
    }

    #[cfg(unix)]
    #[test]
    fn stop_all_kills_two_agent_groups_with_one_shutdown_path() {
        use std::os::unix::process::CommandExt;
        let manager = CodeManager::default();
        let mut children = Vec::new();
        for id in ["shutdown-one", "shutdown-two"] {
            let mut child = std::process::Command::new("/bin/sh").args(["-c", "sleep 600 & wait"]).process_group(0).spawn().unwrap();
            let pgid = child.id();
            let (tx, _rx) = mpsc::unbounded_channel();
            let thread = std::thread::spawn(|| {});
            manager.agents.lock().unwrap().insert(id.into(), Agent {
                tx, stop_requested: Arc::new(AtomicBool::new(false)), thread, stopping: false, launched_as_worker: true,
                process_group: Arc::new(AtomicU32::new(pgid)),
                group_stop_failed: Arc::new(AtomicBool::new(false)),
            });
            children.push((pgid, child));
        }
        manager.stop_all();
        let mut live_groups = Vec::new();
        for (pgid, child) in &mut children {
            let live = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                .block_on(crate::code::acp::agent_group_has_live_members(*pgid)).unwrap();
            if live { live_groups.push(*pgid); unsafe { libc::kill(-(*pgid as i32), libc::SIGKILL); } }
            let _ = child.wait();
        }
        assert!(live_groups.is_empty(), "daemon shutdown left process groups alive: {live_groups:?}");
    }

    #[test]
    fn reused_worker_confirmation_is_session_bound_and_local_human_only() {
        let manager = Arc::new(CodeManager::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, request } = env.event {
                tx.send((env.code_session_id, request_id, request)).unwrap();
            }
        });
        let worker_sink = sink.clone();
        let worker_manager = manager.clone();
        let waiter = std::thread::spawn(move || {
            worker_manager.confirm_reused_worker("worker-session", &worker_sink, None, 2, 1, 1, 0, true, 2, "sweep report")
        });
        let (display_session, request_id, request) = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(display_session, "worker-session");
        assert_eq!(request["toolCall"]["_meta"]["patchbay"]["workerRestartConfirm"]["found"], 2);
        assert_eq!(request["options"].as_array().unwrap().len(), 2);
        assert!(manager.respond_permission(&request_id, Some("confirm_restart".into()), None).is_err());
        assert!(manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("other-session")).is_err());
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("worker-session")).unwrap();
        assert!(waiter.join().unwrap().unwrap());
    }

    fn sweep_report(unknown: usize, remaining: Vec<crate::teams::process_sweep::ProcessRecord>) -> crate::teams::process_sweep::SweepReport {
        crate::teams::process_sweep::SweepReport {
            unknown,
            remaining,
            complete: unknown == 0,
            ..Default::default()
        }
    }

    fn remaining_process(pid: u32, command: &str) -> crate::teams::process_sweep::ProcessRecord {
        crate::teams::process_sweep::ProcessRecord {
            pid,
            uid: 1,
            start_sec: 1,
            start_subsec: 0,
            parent: 1,
            pgid: pid,
            command: command.into(),
            paths: Vec::new(),
            paths_complete: true,
        }
    }

    /// pb-20261121-01: a registered fake agent whose commands the test reads.
    fn fake_agent(manager: &CodeManager, id: &str) -> mpsc::UnboundedReceiver<AgentCmd> {
        let (tx, rx) = mpsc::unbounded_channel();
        manager.agents.lock().unwrap().insert(id.into(), Agent {
            tx,
            stop_requested: Arc::new(AtomicBool::new(false)),
            thread: std::thread::spawn(|| {}),
            stopping: false,
            launched_as_worker: false,
            process_group: Arc::new(AtomicU32::new(0)),
            group_stop_failed: Arc::new(AtomicBool::new(false)),
        });
        rx
    }

    /// A tool-approval card open for `id`, as an agent's `session/request_permission` leaves it.
    fn open_card(manager: &CodeManager, id: &str) {
        let (tx, _rx) = oneshot::channel();
        manager.pending.lock().unwrap().insert(
            "card".into(),
            acp::PendingPermission { session_id: id.into(), target_session_id: None, local_human_only: false, offered: vec![], tx },
        );
    }

    fn user_msg(text: &str) -> acp::UserMessage {
        acp::UserMessage {
            id: "m".into(),
            blocks: vec![serde_json::json!({ "type": "text", "text": text })],
            text: text.into(),
            origin: "user".into(),
            attachments: serde_json::Value::Null,
        }
    }

    /// The text blocks of the next prompt the agent was handed.
    fn next_prompt_blocks(rx: &mut mpsc::UnboundedReceiver<AgentCmd>) -> Vec<String> {
        loop {
            match rx.try_recv().expect("a prompt was sent") {
                AgentCmd::Prompt { msg, .. } => {
                    return msg.blocks.iter().map(|b| b["text"].as_str().unwrap_or_default().to_owned()).collect();
                }
                _ => continue,
            }
        }
    }

    fn assert_told(blocks: &[String], because: &str) {
        assert_eq!(blocks.len(), 2, "the message, then the reminder: {blocks:?}");
        assert_eq!(blocks[0], "carry on", "the message stays first: an agent previews a prompt by its first text");
        assert!(blocks[1].starts_with("<system-reminder>\nPatchbay cancelled your last tool call because "), "{blocks:?}");
        assert!(blocks[1].contains(because), "reason {because:?} missing: {blocks:?}");
        assert!(blocks[1].contains("the user did not reject it. Retry it if still needed.\n</system-reminder>"), "{blocks:?}");
    }

    #[test]
    fn a_plan_limit_stop_with_a_card_open_reminds_the_next_prompt_once() {
        let m = CodeManager::default();
        let mut rx = fake_agent(&m, "s");
        open_card(&m, "s");
        m.cancel_for_limit("s", "the 5-hour usage limit was reached (99%); it resets in 3h 48m", Some(1_800_000_000)).unwrap();
        assert!(matches!(rx.try_recv(), Ok(AgentCmd::Cancel)));
        let sender = m.cancel_notes.take_for_senders("s").expect("the senders are told too");
        assert_eq!((sender.kind, sender.resets_at), ("usage_limit_stop", Some(1_800_000_000)));
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: false, reply: None }).unwrap();
        assert_told(&next_prompt_blocks(&mut rx), "usage limit was reached (99%); it resets in 3h 48m");
        m.send("s", AgentCmd::Prompt { msg: user_msg("again"), force: false, reply: None }).unwrap();
        assert_eq!(next_prompt_blocks(&mut rx), ["again"], "used once, then cleared");
    }

    #[test]
    fn a_send_now_with_a_card_open_reminds_that_message() {
        let m = CodeManager::default();
        let mut rx = fake_agent(&m, "s");
        open_card(&m, "s");
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: true, reply: None }).unwrap();
        assert_told(&next_prompt_blocks(&mut rx), "Send now");
        assert_eq!(m.cancel_notes.take_for_senders("s").map(|n| n.kind), Some("send_now"));
    }

    #[test]
    fn a_stop_press_with_a_card_open_reminds_the_next_prompt() {
        let m = CodeManager::default();
        let mut rx = fake_agent(&m, "s");
        open_card(&m, "s");
        m.send("s", AgentCmd::Cancel).unwrap();
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: false, reply: None }).unwrap();
        assert_told(&next_prompt_blocks(&mut rx), "you pressed Stop");
        assert_eq!(m.cancel_notes.take_for_senders("s").map(|n| n.kind), Some("user_stop"));
    }

    #[test]
    fn a_shutdown_with_a_card_open_leaves_the_note_for_the_restarted_agent() {
        let m = CodeManager::default();
        let _rx = fake_agent(&m, "s");
        open_card(&m, "s");
        m.stop("s");
        // The agent is gone; the manager outlives it, so the next agent's first prompt carries the line.
        let mut rx = fake_agent(&m, "s");
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: false, reply: None }).unwrap();
        assert_told(&next_prompt_blocks(&mut rx), "the session was stopped");
        assert_eq!(m.cancel_notes.take_for_senders("s").map(|n| n.kind), Some("shutdown"));
    }

    /// No card, no tool call cancelled: the agent is not told it lost one. A guard stop still tells
    /// the senders, because their message's turn was cut short.
    #[test]
    fn without_a_card_the_agent_is_not_told_but_a_guard_stop_still_tells_the_senders() {
        let m = CodeManager::default();
        let mut rx = fake_agent(&m, "s");
        m.send("s", AgentCmd::Cancel).unwrap();
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: true, reply: None }).unwrap();
        assert_eq!(next_prompt_blocks(&mut rx), ["carry on"]);
        assert!(m.cancel_notes.take_for_senders("s").is_none());
        m.cancel_for_limit("s", "the 5-hour usage limit was reached (99%); it resets in 1h", None).unwrap();
        m.send("s", AgentCmd::Prompt { msg: user_msg("carry on"), force: false, reply: None }).unwrap();
        assert_eq!(next_prompt_blocks(&mut rx), ["carry on"]);
        assert_eq!(m.cancel_notes.take_for_senders("s").map(|n| n.kind), Some("usage_limit_stop"));
    }

    /// A restart confirmation is a human-only synthetic card, not a tool call.
    #[test]
    fn a_human_only_confirmation_is_not_a_tool_card() {
        let m = CodeManager::default();
        let _rx = fake_agent(&m, "s");
        let (tx, _r) = oneshot::channel();
        m.pending.lock().unwrap().insert(
            "c".into(),
            acp::PendingPermission { session_id: "s".into(), target_session_id: None, local_human_only: true, offered: vec![], tx },
        );
        assert!(!m.has_open_card("s"));
    }

    #[test]
    fn stopping_target_cancels_confirmation_and_invalidates_start() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event {
                tx.send(request_id).unwrap();
            }
        });
        let worker_manager = manager.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = worker_manager.confirm_reused_worker("worker-session", &sink, Some("coordinator"), 0, 0, 0, 0, true, 1, "sweep");
            let _ = done_tx.send(result);
        });
        let request_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.stop("worker-session");
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        if result.is_err() {
            manager.stop("coordinator");
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            panic!("stopping the target did not cancel its pending confirmation");
        }
        assert!(!result.unwrap().unwrap());
        waiter.join().unwrap();
        assert!(!manager.pending.lock().unwrap().contains_key(&request_id));
        assert!(manager.require_current_generation(&generation).is_err());
    }

    #[test]
    fn stop_after_confirmation_prevents_the_launch_generation() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event {
                tx.send(request_id).unwrap();
            }
        });
        let reports = Arc::new(Mutex::new(vec![sweep_report(0, vec![]), sweep_report(0, vec![])]));
        let worker_manager = manager.clone();
        let worker_sink = sink.clone();
        let worker_generation = generation.clone();
        let waiter = std::thread::spawn(move || {
            worker_manager.confirm_reused_worker_with_sweep(
                "worker-session", &worker_sink, Some("coordinator"), &worker_generation, "sweep",
                || Ok(reports.lock().unwrap().remove(0)),
            )
        });
        let request_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("coordinator")).unwrap();
        assert!(waiter.join().unwrap().unwrap());
        // Stop after confirmation/preflight but before the guarded ACP spawn.
        manager.stop("worker-session");
        let mut spawned = false;
        let result = manager.with_spawn_generation(&generation, || spawned = true);
        assert!(result.is_err());
        assert!(!spawned, "cancelled generation must not call the spawn closure");
    }

    #[test]
    fn stopping_confirmation_owner_after_answer_prevents_worker_launch() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        manager.bind_generation_owner(&generation, Some("coordinator")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event { tx.send(request_id).unwrap(); }
        });
        let reports = Arc::new(Mutex::new(vec![sweep_report(0, vec![]), sweep_report(0, vec![])]));
        let worker_manager = manager.clone();
        let worker_sink = sink.clone();
        let worker_generation = generation.clone();
        let waiter = std::thread::spawn(move || worker_manager.confirm_reused_worker_with_sweep(
            "worker-session", &worker_sink, Some("coordinator"), &worker_generation, "sweep",
            || Ok(reports.lock().unwrap().remove(0)),
        ));
        let request_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("coordinator")).unwrap();
        assert!(waiter.join().unwrap().unwrap());
        manager.stop("coordinator");
        let mut spawned = false;
        let result = manager.with_spawn_generation(&generation, || spawned = true);
        assert!(result.is_err());
        assert!(!spawned);
    }

    #[test]
    fn remaining_process_refuses_without_creating_confirmation() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = request_count.clone();
        let responder = manager.clone();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event {
                count.fetch_add(1, Ordering::SeqCst);
                let _ = responder.respond_permission(&request_id, Some("confirm_restart".into()), Some("worker-session"));
            }
        });
        let report = sweep_report(0, vec![remaining_process(77, "worker-helper")]);
        let result = manager.confirm_reused_worker_with_sweep("worker-session", &sink, None, &generation, "sweep", || Ok(report.clone()));
        assert!(result.unwrap_err().to_string().contains("77 (worker-helper)"));
        assert_eq!(request_count.load(Ordering::SeqCst), 0);
        assert!(manager.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_only_confirmation_shows_unchecked_count() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, request } = env.event {
                tx.send((request_id, request)).unwrap();
            }
        });
        let reports = Arc::new(Mutex::new(vec![sweep_report(3, vec![]), sweep_report(3, vec![])]));
        let worker_manager = manager.clone();
        let worker_sink = sink.clone();
        let worker_generation = generation.clone();
        let waiter = std::thread::spawn(move || worker_manager.confirm_reused_worker_with_sweep(
            "worker-session", &worker_sink, None, &worker_generation, "sweep",
            || Ok(reports.lock().unwrap().remove(0)),
        ));
        let (request_id, request) = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(request["toolCall"]["_meta"]["patchbay"]["workerRestartConfirm"]["message"].as_str().unwrap().contains("3 processes could not be checked"));
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("worker-session")).unwrap();
        assert!(waiter.join().unwrap().unwrap());
    }

    #[test]
    fn post_confirmation_remaining_process_refuses() {
        assert_post_confirmation_refused(
            sweep_report(0, vec![]),
            sweep_report(0, vec![remaining_process(78, "leftover")]),
            "78 (leftover)",
        );
    }

    #[test]
    fn post_confirmation_unknown_increase_refuses() {
        assert_post_confirmation_refused(sweep_report(1, vec![]), sweep_report(2, vec![]), "more unchecked processes");
    }

    fn assert_post_confirmation_refused(
        initial: crate::teams::process_sweep::SweepReport,
        after: crate::teams::process_sweep::SweepReport,
        expected: &str,
    ) {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event { tx.send(request_id).unwrap(); }
        });
        let reports = Arc::new(Mutex::new(vec![initial, after]));
        let worker_manager = manager.clone();
        let worker_sink = sink.clone();
        let worker_generation = generation.clone();
        let waiter = std::thread::spawn(move || worker_manager.confirm_reused_worker_with_sweep(
            "worker-session", &worker_sink, None, &worker_generation, "sweep",
            || Ok(reports.lock().unwrap().remove(0)),
        ));
        let request_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("worker-session")).unwrap();
        assert!(waiter.join().unwrap().unwrap_err().to_string().contains(expected));
    }

    #[test]
    fn clean_post_confirmation_resweep_allows_proceeding() {
        let manager = Arc::new(CodeManager::default());
        let generation = manager.new_generation("worker-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { request_id, .. } = env.event { tx.send(request_id).unwrap(); }
        });
        let reports = Arc::new(Mutex::new(vec![sweep_report(0, vec![]), sweep_report(0, vec![])]));
        let worker_manager = manager.clone();
        let worker_sink = sink.clone();
        let worker_generation = generation.clone();
        let waiter = std::thread::spawn(move || worker_manager.confirm_reused_worker_with_sweep(
            "worker-session", &worker_sink, None, &worker_generation, "sweep",
            || Ok(reports.lock().unwrap().remove(0)),
        ));
        let request_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.respond_permission(&request_id, Some("confirm_restart".into()), Some("worker-session")).unwrap();
        assert!(waiter.join().unwrap().unwrap());
    }

    #[test]
    fn stopping_confirmation_owner_cancels_preflight_wait() {
        let manager = Arc::new(CodeManager::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { .. } = env.event { tx.send(()).unwrap(); }
        });
        let waiter_manager = manager.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = waiter_manager.confirm_reused_worker("worker-session", &sink, Some("coordinator"), 0, 0, 0, 0, true, 1, "complete");
            let _ = done_tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        // This models Stop/archive on the coordinator while no UI is attached to answer the card.
        manager.stop("coordinator");
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        if result.is_err() {
            manager.stop("worker-session");
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            panic!("stopping the confirmation owner did not cancel its pending request");
        }
        assert!(!result.unwrap().unwrap());
        waiter.join().unwrap();
        assert!(manager.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_shutdown_cancels_unanswered_confirmation() {
        let manager = Arc::new(CodeManager::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: Sink = Arc::new(move |env| {
            if let acp::CodeEvent::Permission { .. } = env.event { tx.send(()).unwrap(); }
        });
        let waiter_manager = manager.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = waiter_manager.confirm_reused_worker("worker-session", &sink, Some("coordinator"), 0, 0, 0, 2, false, 1, "incomplete");
            let _ = done_tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manager.stop_all();
        assert!(!done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap());
        waiter.join().unwrap();
        assert!(manager.pending.lock().unwrap().is_empty());
    }

    fn insert_pending(m: &CodeManager, id: &str, offered: &[&str]) -> oneshot::Receiver<Option<String>> {
        let (tx, rx) = oneshot::channel();
        m.pending.lock().unwrap().insert(
            id.to_owned(),
            PendingPermission {
                session_id: "w1".into(),
                target_session_id: None,
                local_human_only: false,
                offered: offered.iter().map(|s| (*s).to_owned()).collect(),
                tx,
            },
        );
        rx
    }

    /// P5(a): stripped persistent optionId => Err + still pending.
    #[test]
    fn stripped_option_id_is_refused_and_stays_pending() {
        let m = CodeManager::default();
        let mut rx = insert_pending(&m, "r1", &["allow_once", "cancel"]);
        let err = m
            .respond_permission("r1", Some("accept_execpolicy_amendment".into()), None)
            .expect_err("stripped id must Err");
        assert!(
            err.to_string().contains("option not offered for this request"),
            "{err}"
        );
        assert!(m.pending.lock().unwrap().contains_key("r1"), "must stay pending");
        assert!(rx.try_recv().is_err(), "nothing forwarded to the agent waiter");
    }

    /// P5(b): after a refused stripped id, allow_once completes once.
    #[tokio::test]
    async fn allow_once_after_refused_stripped_id_delivers_once() {
        let m = CodeManager::default();
        let rx = insert_pending(&m, "r1", &["allow_once", "cancel"]);
        m.respond_permission("r1", Some("accept_execpolicy_amendment".into()), None)
            .expect_err("stripped");
        assert!(m.pending.lock().unwrap().contains_key("r1"));
        m.respond_permission("r1", Some("allow_once".into()), Some("w1")).unwrap();
        assert!(!m.pending.lock().unwrap().contains_key("r1"));
        assert_eq!(rx.await.unwrap(), Some("allow_once".into()));
    }

    /// P5(c): unknown garbage optionId => Err + still pending.
    #[test]
    fn unknown_garbage_option_id_is_refused_and_stays_pending() {
        let m = CodeManager::default();
        let _rx = insert_pending(&m, "r1", &["allow_once", "cancel"]);
        let err = m
            .respond_permission("r1", Some("not-a-real-option".into()), None)
            .expect_err("garbage id must Err");
        assert!(err.to_string().contains("option not offered for this request: not-a-real-option"));
        assert!(m.pending.lock().unwrap().contains_key("r1"));
    }

    /// P5(d): refusing a stripped id does not touch Patchbay's Always (permission_rules) store.
    #[test]
    fn refused_stripped_id_does_not_write_permission_rules() {
        let dir = std::env::temp_dir().join(format!("pb-p5-rules-{}", crate::db::new_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        assert!(db.list_permission_rules().unwrap().is_empty());
        let m = CodeManager::default();
        let _rx = insert_pending(&m, "r1", &["allow_once", "cancel"]);
        m.respond_permission("r1", Some("accept_execpolicy_amendment".into()), None)
            .expect_err("stripped");
        m.respond_permission("r1", Some("allow_always".into()), None)
            .expect_err("allow_always stripped");
        assert!(
            db.list_permission_rules().unwrap().is_empty(),
            "Patchbay Always rule store must stay unchanged"
        );
        assert!(m.pending.lock().unwrap().contains_key("r1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// G3: wrong session id => Err + still pending.
    #[test]
    fn respond_permission_wrong_session_stays_pending() {
        let m = CodeManager::default();
        let _rx = insert_pending(&m, "r1", &["allow_once", "cancel"]);
        let err = m
            .respond_permission("r1", Some("allow_once".into()), Some("other-session"))
            .expect_err("wrong session");
        assert!(err.to_string().contains("session does not own this permission request"));
        assert!(m.pending.lock().unwrap().contains_key("r1"));
    }
}
