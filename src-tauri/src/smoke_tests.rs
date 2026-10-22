//! End-to-end smoke tests against `scripts/mock-llm.py`.
//! Run: `scripts/mock-llm.py &` then `cargo test --lib smoke -- --ignored --nocapture`.

use std::sync::Arc;

use patchbay_llm::{Client, Message, Part, Request, Role, SystemBlock};
use tokio_util::sync::CancellationToken;

use crate::db::{Db, Model, Provider, ProviderKind};
use crate::providers::{self, Resolved};

fn mock_url() -> String {
    std::env::var("PATCHBAY_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8765/v1".into())
}

fn mock_resolved(kind: ProviderKind) -> Resolved {
    Resolved {
        provider: Provider {
            id: "p".into(),
            name: "mock".into(),
            kind,
            base_url: mock_url(),
            has_key: true,
            auth_mode: Default::default(),
        },
        model: Model {
            id: "m".into(),
            provider_id: "p".into(),
            model_id: "mock-echo".into(),
            display_name: "Mock".into(),
            vision: true,
            context_window: None,
            params: Default::default(),
        },
        key: Some("test-key".into()),
        auth: providers::Auth::Key,
    }
}

async fn sample(kind: ProviderKind, parts: Vec<Part>) -> String {
    let r = mock_resolved(kind);
    let req = Request {
        system: vec![SystemBlock::new("be terse")],
        messages: vec![Message::with_parts(Role::User, parts)],
        ..Default::default()
    };
    Client::new(providers::llm_config(&r))
        .complete_text(&req, &CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("sampling failed: {e}"))
}

fn parts() -> Vec<Part> {
    vec![
        Part::Text("<file name=\"a.csv\" type=\"text/csv\">\na,b\n</file>".into()),
        Part::Image { mime: "image/png".into(), data: "iVBORw0KGgo=".into() },
        Part::Text("what is this?".into()),
    ]
}

#[tokio::test]
#[ignore]
async fn smoke_chat_openai() {
    let out = sample(ProviderKind::OpenaiChat, parts()).await;
    println!("{out}");
    assert!(out.contains("what is this?"), "{out}");
    assert!(out.contains("images seen: 1"), "{out}");
    assert!(out.contains("a.csv"), "{out}");
}

#[tokio::test]
#[ignore]
async fn smoke_chat_anthropic() {
    let out = sample(ProviderKind::Anthropic, parts()).await;
    println!("{out}");
    assert!(out.contains("images seen: 1"), "{out}");
}

#[tokio::test]
#[ignore]
async fn smoke_generate_title() {
    // The mock echoes the prompt, so the title is the first line of the echo.
    let t = crate::chat::generate_title(&mock_resolved(ProviderKind::OpenaiChat), "how do lifetimes work", "They scope borrows.")
        .await
        .expect("title call");
    println!("{t:?}");
    assert!(t.is_some_and(|t| !t.is_empty() && t.chars().count() <= 61));
}

#[tokio::test]
#[ignore]
async fn smoke_anthropic_pdf_document() {
    let pdf = Part::Document { mime: "application/pdf".into(), name: "a.pdf".into(), data: "JVBERi0xLjQK".into() };
    let out = sample(ProviderKind::Anthropic, vec![pdf, Part::Text("summarize".into())]).await;
    println!("{out}");
    assert!(out.contains("[document application/pdf"), "{out}");
    assert!(out.contains("images seen: 0"), "{out}");
}

/// Writes a GROK_HOME for manual ACP probing; prints its path and env.
#[test]
#[ignore]
fn smoke_render_grok_config() {
    let dir = std::env::temp_dir().join("patchbay-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("db.sqlite")).unwrap();
    for (id, kind) in [("oa", ProviderKind::OpenaiChat), ("an", ProviderKind::Anthropic)] {
        let p = Provider {
            id: id.into(),
            name: id.into(),
            kind,
            base_url: mock_url(),
            has_key: false,
            auth_mode: Default::default(),
        };
        db.upsert_provider(&p, Some("test-key")).unwrap();
        db.upsert_model(&Model {
            id: format!("{id}-m"),
            provider_id: id.into(),
            model_id: "mock-agent".into(),
            display_name: format!("Mock agent ({id})"),
            vision: true,
            context_window: Some(32000),
            params: Default::default(),
        })
        .unwrap();
    }
    let r = crate::code::grok_config::render(&db, Some("oa-m")).unwrap();
    crate::code::grok_config::write(&dir.join("grok"), &r).unwrap();
    println!("{}", r.toml);
    for (k, _) in &r.env {
        println!("env {k}");
    }
    println!("GROK_HOME={}", dir.join("grok").display());
}

/// Full Code-tab path: CodeManager -> ACP client -> Grok Build sidecar -> mock model.
#[test]
#[ignore]
fn smoke_code_agent_edit_with_permission() {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::code::CodeManager;
    use crate::code::acp::{AgentCmd, CodeEvent, Envelope, Sink};

    let root = std::env::temp_dir().join("patchbay-smoke-code");
    let _ = std::fs::remove_dir_all(&root);
    let data_dir = root.join("data");
    let proj = root.join("proj");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("README.md"), "# smoke\n").unwrap();

    let db = Arc::new(Db::open(&data_dir.join("db.sqlite")).unwrap());
    let kind = match std::env::var("PATCHBAY_SMOKE_KIND").as_deref() {
        Ok("openai") => ProviderKind::OpenaiChat,
        _ => ProviderKind::Anthropic,
    };
    db.upsert_provider(
        &Provider {
            id: "p".into(),
            name: "mock".into(),
            kind,
            base_url: mock_url(),
            has_key: false,
            auth_mode: Default::default(),
        },
        Some("test-key"),
    )
    .unwrap();
    db.upsert_model(&Model {
        id: "m".into(),
        provider_id: "p".into(),
        model_id: "mock-agent".into(),
        display_name: "Mock agent".into(),
        vision: true,
        context_window: Some(32000),
        params: crate::db::ModelParams {
            max_output_tokens: Some(1234),
            temperature: Some(0.3),
            top_p: Some(0.9),
            reasoning_effort: Some("low".into()),
            auto_compact_percent: Some(80),
            ..Default::default()
        },
    })
    .unwrap();
    let session = db
        .create_code_session(proj.to_str().unwrap(), Some("m"), "grok", "default")
        .unwrap();
    crate::prefs::save(
        &db,
        &crate::prefs::Preferences {
            agent_instructions: "PB-RULE-MARKER: be terse.".into(),
            web_fetch: true,
            background_summaries: false,
            auto_compact_percent: Some(75),
            ..Default::default()
        },
    )
    .unwrap();

    let manager = Arc::new(CodeManager::default());
    let events: Arc<Mutex<Vec<Envelope>>> = Arc::default();
    let sink: Sink = {
        let events = events.clone();
        let manager = manager.clone();
        Arc::new(move |env: Envelope| {
            if let CodeEvent::Permission { request_id, request } = &env.event {
                let allow = request["options"]
                    .as_array()
                    .and_then(|o| o.iter().find(|x| x["kind"] == "allow_once"))
                    .and_then(|x| x["optionId"].as_str())
                    .map(str::to_owned);
                manager.respond_permission(request_id, allow);
            }
            events.lock().unwrap().push(env);
        })
    };
    manager
        .start(sink, db.clone(), &data_dir, &session.id)
        .unwrap();

    let wait_for = |pred: &dyn Fn(&CodeEvent) -> bool, what: &str| {
        let t0 = Instant::now();
        loop {
            if events.lock().unwrap().iter().any(|e| pred(&e.event)) {
                return;
            }
            if t0.elapsed() > Duration::from_secs(90) {
                panic!("timed out waiting for {what}: {:#?}", events.lock().unwrap());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for(
        &|e| matches!(e, CodeEvent::Status { state, .. } if state == "ready"),
        "ready",
    );
    assert!(
        db.get_code_session(&session.id)
            .unwrap()
            .unwrap()
            .acp_session_id
            .is_some()
    );

    manager
        .send(
            &session.id,
            AgentCmd::prompt("please !edit"),
        )
        .unwrap();
    wait_for(&|e| matches!(e, CodeEvent::PromptDone { .. }), "prompt done");
    manager.stop(&session.id);

    let evs = events.lock().unwrap();
    for e in evs.iter() {
        match &e.event {
            CodeEvent::Update { update } => println!(
                "update {} {}",
                update["sessionUpdate"],
                update.get("status").cloned().unwrap_or_default()
            ),
            other => println!("{other:?}"),
        }
    }
    let agent_text: String = evs
        .iter()
        .filter_map(|e| match &e.event {
            CodeEvent::Update { update } if update["sessionUpdate"] == "agent_message_chunk" => {
                update["content"]["text"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect();
    assert!(
        agent_text.contains("custom instructions: seen"),
        "agent instructions did not reach the model: {agent_text}"
    );
    let errors: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e.event, CodeEvent::Error { .. }))
        .collect();
    assert!(errors.is_empty(), "agent reported errors: {errors:#?}");
    assert!(evs.iter().any(|e| matches!(e.event, CodeEvent::Permission { .. })));
    let usage = evs
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            CodeEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("no usage event (status line not enabled?)");
    println!("usage: {usage:?}");
    assert_eq!(usage.context_window, Some(32000));
    assert_eq!(usage.auto_compact_percent, Some(80), "auto-compact param not applied");
    assert!(usage.context_tokens.unwrap_or(0) > 0);
    assert!(usage.totals.input > 0 && usage.totals.output > 0);
    let before = usage.totals;
    assert!(evs.iter().any(|e| matches!(&e.event, CodeEvent::Update { update }
        if update["content"].as_array().is_some_and(|c| c.iter().any(|x| x["type"] == "diff")))));
    assert_eq!(
        std::fs::read_to_string(proj.join("patchbay-test.txt")).unwrap(),
        "hello from patchbay\n"
    );
    drop(evs);

    // Restart: the stored ACP session id must be loaded and its history replayed.
    std::thread::sleep(Duration::from_millis(500));
    events.lock().unwrap().clear();
    let sink2: Sink = {
        let events = events.clone();
        Arc::new(move |env: Envelope| events.lock().unwrap().push(env))
    };
    manager
        .start(sink2, db.clone(), &data_dir, &session.id)
        .unwrap();
    wait_for(
        &|e| matches!(e, CodeEvent::SessionReady { loaded: true, .. }),
        "resumed session",
    );
    let evs = events.lock().unwrap();
    assert!(
        evs.iter().any(|e| matches!(&e.event, CodeEvent::Update { update }
            if update["sessionUpdate"] == "user_message_chunk")),
        "no replayed user message: {evs:#?}"
    );
    drop(evs);
    manager.stop(&session.id);
    // Grok's ledger restarts at zero in the new process; ours must not.
    let after = db.code_usage(&session.id).unwrap().view().totals;
    assert!(after.input >= before.input && after.output >= before.output, "{before:?} -> {after:?}");
}

/// Rewind via `x.ai/rewind/*`: dropping the first prompt reverts its file edit and its history.
#[test]
#[ignore]
fn smoke_code_rewind() {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::code::CodeManager;
    use crate::code::acp::{AgentCmd, CodeEvent, Envelope, Sink};

    let root = std::env::temp_dir().join("patchbay-smoke-rewind");
    let _ = std::fs::remove_dir_all(&root);
    let data_dir = root.join("data");
    let proj = root.join("proj");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("README.md"), "# smoke\n").unwrap();
    let db = Arc::new(Db::open(&data_dir.join("db.sqlite")).unwrap());
    db.upsert_provider(
        &Provider {
            id: "p".into(),
            name: "mock".into(),
            kind: ProviderKind::OpenaiChat,
            base_url: mock_url(),
            has_key: false,
            auth_mode: Default::default(),
        },
        Some("test-key"),
    )
    .unwrap();
    db.upsert_model(&Model {
        id: "m".into(),
        provider_id: "p".into(),
        model_id: "mock-agent".into(),
        display_name: "Mock agent".into(),
        vision: true,
        context_window: Some(32000),
        params: Default::default(),
    })
    .unwrap();
    let session = db
        .create_code_session(proj.to_str().unwrap(), Some("m"), "grok", "default")
        .unwrap();

    let manager = Arc::new(CodeManager::default());
    let events: Arc<Mutex<Vec<Envelope>>> = Arc::default();
    let make_sink = || -> Sink {
        let events = events.clone();
        let manager = manager.clone();
        Arc::new(move |env: Envelope| {
            if let CodeEvent::Permission { request_id, .. } = &env.event {
                manager.respond_permission(request_id, Some("allow-once".into()));
            }
            events.lock().unwrap().push(env);
        })
    };
    let wait_count = |pred: &dyn Fn(&CodeEvent) -> bool, n: usize, what: &str| {
        let t0 = Instant::now();
        while events.lock().unwrap().iter().filter(|e| pred(&e.event)).count() < n {
            assert!(t0.elapsed() < Duration::from_secs(90), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let is_done = |e: &CodeEvent| matches!(e, CodeEvent::PromptDone { .. });
    let prompt = |text: &str| {
        manager
            .send(
                &session.id,
                AgentCmd::prompt(text),
            )
            .unwrap()
    };

    manager.start(make_sink(), db.clone(), &data_dir, &session.id).unwrap();
    wait_count(&|e| matches!(e, CodeEvent::Status { state, .. } if state == "ready"), 1, "ready");
    prompt("please !edit");
    wait_count(&is_done, 1, "first prompt");
    prompt("second message");
    wait_count(&is_done, 2, "second prompt");
    assert!(proj.join("patchbay-test.txt").exists());

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(manager.rewind(&session.id, 0, "please !edit".into(), true))
        .expect("rewind");
    assert!(
        !proj.join("patchbay-test.txt").exists(),
        "file edit was not reverted"
    );
    manager.stop(&session.id);

    std::thread::sleep(Duration::from_millis(500));
    events.lock().unwrap().clear();
    manager.start(make_sink(), db.clone(), &data_dir, &session.id).unwrap();
    wait_count(
        &|e| matches!(e, CodeEvent::SessionReady { loaded: true, .. }),
        1,
        "resume",
    );
    std::thread::sleep(Duration::from_millis(300));
    let users: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            CodeEvent::Update { update } if update["sessionUpdate"] == "user_message_chunk" => {
                Some(update["content"]["text"].as_str().unwrap_or("").to_owned())
            }
            _ => None,
        })
        .collect();
    manager.stop(&session.id);
    assert!(users.is_empty(), "history not truncated: {users:?}");
}

/// Saved rules answer permission requests, and configured MCP servers reach the agent.
#[test]
#[ignore]
fn smoke_code_rules_and_mcp() {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::code::CodeManager;
    use crate::code::acp::{AgentCmd, CodeEvent, Envelope, Sink};
    use crate::code::mcp::{McpConfig, save};

    let root = std::env::temp_dir().join("patchbay-smoke-rules");
    let _ = std::fs::remove_dir_all(&root);
    let data_dir = root.join("data");
    let proj = root.join("proj");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    let db = Arc::new(Db::open(&data_dir.join("db.sqlite")).unwrap());
    db.upsert_provider(
        &Provider { id: "p".into(), name: "mock".into(), kind: ProviderKind::OpenaiChat, base_url: mock_url(), has_key: false, auth_mode: Default::default(), },
        Some("test-key"),
    )
    .unwrap();
    db.upsert_model(&Model {
        id: "m".into(),
        provider_id: "p".into(),
        model_id: "mock-agent".into(),
        display_name: "Mock agent".into(),
        vision: true,
        context_window: Some(32000),
        params: Default::default(),
    })
    .unwrap();
    let session = db.create_code_session(proj.to_str().unwrap(), Some("m"), "grok", "default").unwrap();
    db.add_permission_rule(proj.to_str().unwrap(), "edit", "").unwrap();
    let mock_mcp = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/mock-mcp.py");
    let mcp_log = root.join("mcp.log");
    save(
        &db,
        None,
        "pbmock",
        true,
        McpConfig {
            transport: "stdio".into(),
            command: "python3".into(),
            args: vec![mock_mcp.to_string_lossy().into_owned()],
            env: vec![crate::code::mcp::KeyValue {
                name: "PB_MOCK_MCP_LOG".into(),
                value: mcp_log.to_string_lossy().into_owned(),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .unwrap();

    let manager = Arc::new(CodeManager::default());
    let events: Arc<Mutex<Vec<Envelope>>> = Arc::default();
    let sink: Sink = {
        let events = events.clone();
        // No permission responder: a request that reaches the UI would hang the test.
        Arc::new(move |env: Envelope| events.lock().unwrap().push(env))
    };
    let wait_count = |pred: &dyn Fn(&CodeEvent) -> bool, n: usize, what: &str| {
        let t0 = Instant::now();
        while events.lock().unwrap().iter().filter(|e| pred(&e.event)).count() < n {
            assert!(t0.elapsed() < Duration::from_secs(90), "timed out: {what}: {:#?}", events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let is_done = |e: &CodeEvent| matches!(e, CodeEvent::PromptDone { .. });
    let prompt = |text: &str| {
        manager
            .send(&session.id, AgentCmd::prompt(text))
            .unwrap()
    };
    manager.start(sink, db.clone(), &data_dir, &session.id).unwrap();
    wait_count(&|e| matches!(e, CodeEvent::Status { state, .. } if state == "ready"), 1, "ready");
    prompt("please !edit");
    wait_count(&is_done, 1, "edit prompt");
    prompt("which !tools");
    wait_count(&is_done, 2, "tools prompt");
    manager.stop(&session.id);

    let evs = events.lock().unwrap();
    let summary: Vec<String> = evs
        .iter()
        .map(|e| match &e.event {
            CodeEvent::Update { update } => format!(
                "{} {} {}",
                update["sessionUpdate"].as_str().unwrap_or(""),
                update["kind"].as_str().unwrap_or(""),
                update["status"].as_str().unwrap_or("")
            ),
            other => format!("{other:?}").chars().take(120).collect(),
        })
        .filter(|l| !l.starts_with("agent_message_chunk") && !l.starts_with("available_commands"))
        .collect();
    assert!(
        evs.iter().any(|e| matches!(e.event, CodeEvent::AutoApproved { .. })),
        "rule did not fire: {summary:#?}\n{}",
        evs.iter()
            .filter_map(|e| match &e.event {
                CodeEvent::Update { update } if update["sessionUpdate"] == "agent_message_chunk" => update["content"]["text"].as_str(),
                _ => None,
            })
            .collect::<String>()
    );
    assert!(!evs.iter().any(|e| matches!(e.event, CodeEvent::Permission { .. })));
    assert!(proj.join("patchbay-test.txt").exists());
    let caps = evs.iter().find_map(|e| match &e.event {
        CodeEvent::SessionReady { capabilities, .. } => Some(capabilities.clone()),
        _ => None,
    });
    println!("capabilities: {caps:?}");
    let text: String = evs
        .iter()
        .filter_map(|e| match &e.event {
            CodeEvent::Update { update } if update["sessionUpdate"] == "agent_message_chunk" => {
                update["content"]["text"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect();
    println!("{text}");
    // Grok defers MCP tools behind `search_tool`; the server must still be started and listed.
    let log = std::fs::read_to_string(&mcp_log).unwrap_or_default();
    assert!(log.contains("initialize") && log.contains("tools/list"), "MCP server not used: {log:?}");
}

/// What the agent picks up from `~/.claude` follows the Claude-config preferences.
/// Uses the real home directory, so it only asserts when you have Claude skills or plugins.
#[test]
#[ignore]
fn smoke_claude_config_toggle() {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::code::CodeManager;
    use crate::code::acp::{CodeEvent, Envelope, Sink};

    let commands_with = |skills: bool| -> Vec<String> {
        let root = std::env::temp_dir().join(format!("patchbay-smoke-claude-{skills}"));
        let plugins = skills;
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("data");
        let proj = root.join("proj");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        let db = Arc::new(Db::open(&data_dir.join("db.sqlite")).unwrap());
        db.upsert_provider(
            &Provider { id: "p".into(), name: "mock".into(), kind: ProviderKind::OpenaiChat, base_url: mock_url(), has_key: false, auth_mode: Default::default(), },
            Some("test-key"),
        )
        .unwrap();
        db.upsert_model(&Model {
            id: "m".into(),
            provider_id: "p".into(),
            model_id: "mock-agent".into(),
            display_name: "Mock agent".into(),
            vision: true,
            context_window: Some(32000),
            params: Default::default(),
        })
        .unwrap();
        let mut prefs = crate::prefs::Preferences::default();
        prefs.claude_config.skills = skills;
        prefs.claude_config.plugins = plugins;
        crate::prefs::save(&db, &prefs).unwrap();
        let session = db.create_code_session(proj.to_str().unwrap(), Some("m"), "grok", "default").unwrap();
        let manager = Arc::new(CodeManager::default());
        let events: Arc<Mutex<Vec<Envelope>>> = Arc::default();
        let sink: Sink = {
            let events = events.clone();
            Arc::new(move |env: Envelope| events.lock().unwrap().push(env))
        };
        manager.start(sink, db.clone(), &data_dir, &session.id).unwrap();
        let t0 = Instant::now();
        while !events.lock().unwrap().iter().any(|e| matches!(&e.event, CodeEvent::Status { state, .. } if state == "ready")) {
            assert!(t0.elapsed() < Duration::from_secs(60), "agent did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        // Session-level updates arrive shortly after ready.
        std::thread::sleep(Duration::from_millis(1500));
        manager.stop(&session.id);
        let evs = events.lock().unwrap();
        let mut names: Vec<String> = evs
            .iter()
            .filter_map(|e| match &e.event {
                CodeEvent::Update { update } if update["sessionUpdate"] == "available_commands_update" => {
                    update["availableCommands"].as_array().cloned()
                }
                _ => None,
            })
            .next_back()
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    };
    let on = commands_with(true);
    let off = commands_with(false);
    let only_on: Vec<&String> = on.iter().filter(|n| !off.contains(n)).collect();
    println!("on: {} commands, off: {} commands", on.len(), off.len());
    println!("only with Claude skills: {only_on:?}");
    println!("off: {off:?}");
    let off_extra: Vec<&String> = off.iter().filter(|n| !on.contains(n)).collect();
    assert!(off_extra.is_empty(), "turning skills off added commands: {off_extra:?}");
    // Every user skill dir and installed Claude plugin must be gone when off.
    let home = std::env::var("HOME").unwrap_or_default();
    let skill_dirs: Vec<String> = std::fs::read_dir(std::path::Path::new(&home).join(".claude/skills/synced"))
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let installed: Vec<String> = std::fs::read_to_string(std::path::Path::new(&home).join(".claude/plugins/installed_plugins.json"))
        .ok()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .and_then(|v| v["plugins"].as_object().map(|o| o.keys().map(|k| k.split('@').next().unwrap_or(k).to_owned()).collect()))
        .unwrap_or_default();
    let leaked: Vec<&String> = off
        .iter()
        .filter(|n| skill_dirs.iter().any(|s| n.ends_with(s.as_str())) || installed.iter().any(|p| n.starts_with(p.as_str())))
        .collect();
    assert!(leaked.is_empty(), "Claude skills or plugins still loaded with the setting off: {leaked:?}");
}
