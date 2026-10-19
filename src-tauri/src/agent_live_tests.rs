//! Live checks against the third-party coding agents installed on this machine (Claude Code,
//! OpenCode, Cursor CLI, Kilo Code, Codex, Gemini CLI, Antigravity). They use the agents' own
//! sign-ins, so they are ignored by default and skip agents that aren't installed.
//!
//! Handshake only (start, models, switch model, resume):
//!   `cargo test --lib agents_live -- --ignored --nocapture`
//! Also send one tiny prompt to the agents listed (this spends a little of their quota):
//!   `PATCHBAY_LIVE_PROMPT=opencode,claude-code cargo test --lib agents_live -- --ignored --nocapture`
//! Limit the agents tried: `PATCHBAY_LIVE_AGENTS=opencode,cursor`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::code::CodeManager;
use crate::code::acp::{AgentCmd, CodeEvent, Envelope, Sink};
use crate::code::agents::{self, AgentKind};
use crate::db::Db;

fn listed(var: &str, kind: AgentKind) -> Option<bool> {
    std::env::var(var)
        .ok()
        .map(|v| v.split(',').any(|s| s.trim() == kind.id()))
}

struct Run {
    db: Arc<Db>,
    data_dir: std::path::PathBuf,
    manager: Arc<CodeManager>,
    events: Arc<Mutex<Vec<Envelope>>>,
    session_id: String,
}

impl Run {
    fn new(kind: AgentKind, root: &std::path::Path) -> Run {
        let data_dir = root.join("data");
        let proj = root.join("proj");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("README.md"), "# live test\n").unwrap();
        let db = Arc::new(Db::open(&data_dir.join("db.sqlite")).unwrap());
        let session = db
            .create_code_session(proj.to_str().unwrap(), None, kind.id(), "default")
            .unwrap();
        Run {
            db,
            data_dir,
            manager: Arc::new(CodeManager::default()),
            events: Arc::default(),
            session_id: session.id,
        }
    }

    fn start(&self) {
        // Let the previous process exit first, so its "exited" event isn't read as this one's.
        self.manager.stop_and_wait(&self.session_id, Duration::from_secs(5));
        self.events.lock().unwrap().clear();
        let sink: Sink = {
            let events = self.events.clone();
            let manager = self.manager.clone();
            Arc::new(move |env: Envelope| {
                // Never let a live agent change anything: reject every permission request.
                if let CodeEvent::Permission { request_id, request } = &env.event {
                    let reject = request["options"]
                        .as_array()
                        .and_then(|o| o.iter().find(|x| x["kind"] == "reject_once"))
                        .and_then(|x| x["optionId"].as_str())
                        .map(str::to_owned);
                    manager.respond_permission(request_id, reject);
                }
                events.lock().unwrap_or_else(|e| e.into_inner()).push(env);
            })
        };
        self.manager
            .start(sink, self.db.clone(), &self.data_dir, &self.session_id)
            .unwrap();
    }

    fn wait_for<T>(&self, what: &str, secs: u64, f: impl Fn(&CodeEvent) -> Option<T>) -> T {
        let t0 = Instant::now();
        loop {
            {
                let ev = self.events.lock().unwrap();
                if let Some(t) = ev.iter().find_map(|e| f(&e.event)) {
                    return t;
                }
                let exited = ev
                    .iter()
                    .any(|e| matches!(&e.event, CodeEvent::Status { state, .. } if state == "exited"));
                if exited {
                    let errors: Vec<&str> = ev
                        .iter()
                        .filter_map(|e| match &e.event {
                            CodeEvent::Error { message } => Some(message.as_str()),
                            _ => None,
                        })
                        .collect();
                    let msg = format!("agent exited while waiting for {what}: {errors:?}");
                    drop(ev);
                    self.manager.stop_all();
                    panic!("{msg}");
                }
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                let msg = format!("timed out waiting for {what}: {:#?}", self.events.lock().unwrap());
                self.manager.stop_all();
                panic!("{msg}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn ready(&self) -> (serde_json::Value, bool) {
        self.wait_for("sessionReady", 120, |e| match e {
            CodeEvent::SessionReady { models, loaded, .. } => Some((models.clone(), *loaded)),
            _ => None,
        })
    }
}

fn live(kind: AgentKind) {
    if listed("PATCHBAY_LIVE_AGENTS", kind) == Some(false) {
        return;
    }
    if let Err(e) = agents::resolve(kind, &Default::default()) {
        eprintln!("skip {}: {e:#}", kind.id());
        return;
    }
    let root = std::env::temp_dir().join(format!("pb-live-{}-{}", kind.id(), std::process::id()));
    let run = Run::new(kind, &root);
    let rt = tokio::runtime::Runtime::new().unwrap();

    run.start();
    let (models, loaded) = run.ready();
    assert!(!loaded, "first start creates a session");
    // The agent's own approval presets are hidden, so the mode it runs in must be a shown one.
    let (modes, config_options) = run.wait_for("sessionReady", 5, |e| match e {
        CodeEvent::SessionReady { modes, config_options, .. } => Some((modes.clone(), config_options.clone())),
        _ => None,
    });
    let ids = |v: &serde_json::Value, list: &str, key: &str| -> Vec<String> {
        v[list].as_array().into_iter().flatten().filter_map(|m| m[key].as_str().map(str::to_owned)).collect()
    };
    let shown = ids(&modes, "availableModes", "id");
    let mode = modes["currentModeId"].as_str().unwrap_or_default();
    let options: Vec<String> = config_options.as_array().into_iter().flatten().filter_map(|o| o["id"].as_str().map(str::to_owned)).collect();
    eprintln!("{}: mode {mode} of {shown:?}, options {options:?}", kind.id());
    if !shown.is_empty() {
        assert!(shown.iter().any(|m| m == mode), "{}: runs in hidden mode {mode}", kind.id());
    }
    let available: Vec<String> = models["availableModels"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: no model list: {models}", kind.id()))
        .iter()
        .filter_map(|m| m["modelId"].as_str().map(str::to_owned))
        .collect();
    let current = models["currentModelId"].as_str().unwrap_or_default().to_owned();
    eprintln!("{}: {} models, current {current}", kind.id(), available.len());

    // Switch to another model; it should be persisted and reselected after a restart.
    let other = available.iter().find(|m| **m != current).cloned();
    if let Some(other) = &other {
        rt.block_on(run.manager.set_model(&run.session_id, other.clone())).unwrap();
        run.db.set_code_session_agent_model(&run.session_id, other).unwrap();
    }

    if listed("PATCHBAY_LIVE_PROMPT", kind) == Some(true) {
        run.manager
            .send(
                &run.session_id,
                AgentCmd::Prompt {
                    blocks: vec![serde_json::json!({ "type": "text", "text": "Reply with exactly the word pong and nothing else. Do not use any tools." })],
                },
            )
            .unwrap();
        let stop = run.wait_for("promptDone", 180, |e| match e {
            CodeEvent::PromptDone { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        });
        let text: String = run
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.event {
                CodeEvent::Update { update } if update["sessionUpdate"] == "agent_message_chunk" => {
                    update["content"]["text"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect();
        let errors: Vec<String> = run
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.event {
                CodeEvent::Error { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        eprintln!("{}: stop {stop}, reply {text:?}, errors {errors:?}", kind.id());
        assert_eq!(stop, "end_turn");
        assert!(text.to_lowercase().contains("pong"), "{}: reply {text:?}", kind.id());
        let usage = run.events.lock().unwrap().iter().rev().find_map(|e| match &e.event {
            CodeEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        });
        eprintln!("{}: usage {usage:?}", kind.id());
    }

    // Restart: the session resumes and the chosen model comes back.
    let had_session = run.db.get_code_session(&run.session_id).unwrap().unwrap().acp_session_id;
    assert!(had_session.is_some(), "ACP session id saved");
    run.start();
    let (models, loaded) = run.ready();
    eprintln!("{}: resumed loaded={loaded}", kind.id());
    if let Some(other) = &other {
        assert_eq!(models["currentModelId"].as_str(), Some(other.as_str()), "{}: model restored", kind.id());
    }
    run.manager.stop_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore]
fn agents_live_claude_code() {
    live(AgentKind::ClaudeCode);
}

#[test]
#[ignore]
fn agents_live_opencode() {
    live(AgentKind::Opencode);
}

#[test]
#[ignore]
fn agents_live_cursor() {
    live(AgentKind::Cursor);
}

#[test]
#[ignore]
fn agents_live_kilo() {
    live(AgentKind::Kilo);
}

#[test]
#[ignore]
fn agents_live_codex() {
    live(AgentKind::Codex);
}

#[test]
#[ignore]
fn agents_live_gemini() {
    live(AgentKind::Gemini);
}

#[test]
#[ignore]
fn agents_live_antigravity() {
    live(AgentKind::Antigravity);
}
