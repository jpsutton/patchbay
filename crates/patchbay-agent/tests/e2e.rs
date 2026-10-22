//! The agent against `scripts/mock-llm.py`, driven like an ACP client would.
//! Run: `scripts/mock-llm.py --port 8765 &`, then `cargo test -p patchbay-agent -- --ignored`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use patchbay_agent::{Host, ModelClient, ModelInfo};
use patchbay_llm::{Client, Config, WireFormat};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct TestHost {
    format: WireFormat,
    data: PathBuf,
    window: Option<u64>,
    percent: Option<u8>,
}

#[async_trait]
impl Host for TestHost {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo { id: "pb-mock".into(), name: "Mock".into() }]
    }

    async fn model_client(&self, _: &str) -> Result<ModelClient, String> {
        let base = std::env::var("PATCHBAY_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8765/v1".into());
        let mut cfg = Config::new(self.format, base, "mock-agent");
        cfg.key = Some("test-key".into());
        Ok(ModelClient { client: Client::new(cfg), system_prefix: Vec::new(), context_window: self.window, auto_compact_percent: self.percent })
    }

    fn data_dir(&self) -> PathBuf {
        self.data.clone()
    }

    fn web_search_key(&self) -> Option<String> {
        Some("brave-test-key".into())
    }
}

/// A minimal ACP client over the agent's pipes.
struct Acp {
    to: tokio::io::DuplexStream,
    from: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    next: u64,
    /// Every `session/update` seen so far.
    updates: Vec<Value>,
    /// Permission requests seen, and what to answer them.
    asked: Vec<Value>,
    answer: &'static str,
    /// An option id to pick instead of allow/reject.
    pick: Option<&'static str>,
}

impl Acp {
    fn start(format: WireFormat, data: PathBuf) -> Acp {
        Acp::start_with(format, data, Some(100_000), None)
    }

    fn start_with(format: WireFormat, data: PathBuf, window: Option<u64>, percent: Option<u8>) -> Acp {
        let (c_to_a, a_from_c) = tokio::io::duplex(1 << 20);
        let (a_to_c, c_from_a) = tokio::io::duplex(1 << 20);
        let host = Arc::new(TestHost { format, data, window, percent });
        tokio::spawn(patchbay_agent::serve(host, a_from_c, a_to_c));
        Acp { to: c_to_a, from: BufReader::new(c_from_a).lines(), next: 1, updates: Vec::new(), asked: Vec::new(), answer: "allow", pick: None }
    }

    async fn send(&mut self, v: Value) {
        self.to.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(60), self.from.next_line())
                .await
                .expect("agent answered in time")
                .unwrap()
                .expect("agent closed");
            let m: Value = serde_json::from_str(&line).unwrap();
            if m["id"] == json!(id) && m.get("method").is_none() {
                assert!(m.get("error").is_none(), "{method}: {}", m["error"]);
                return m["result"].clone();
            }
            match m["method"].as_str() {
                Some("session/update") => self.updates.push(m["params"]["update"].clone()),
                Some("session/request_permission") => {
                    self.asked.push(m["params"].clone());
                    let option = self.pick.unwrap_or(if self.answer == "allow" { "allow" } else { "reject" });
                    self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": option}}})).await;
                }
                _ => {}
            }
        }
    }

    fn text_of(&self, kind: &str) -> String {
        self.updates
            .iter()
            .filter(|u| u["sessionUpdate"] == kind)
            .filter_map(|u| u["content"]["text"].as_str())
            .collect()
    }
}

fn workdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("pb-e2e-{}", uuid_like()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

async fn open(acp: &mut Acp, cwd: &PathBuf) -> String {
    let init = acp.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    let s = acp.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
    assert_eq!(s["models"]["currentModelId"], "pb-mock");
    s["sessionId"].as_str().unwrap().to_owned()
}

async fn prompt(acp: &mut Acp, sid: &str, text: &str) -> Value {
    acp.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]})).await
}

async fn edit_flow(format: WireFormat) {
    let cwd = workdir();
    let mut acp = Acp::start(format, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;

    // An edit asks first, and writes the file once allowed.
    let r = prompt(&mut acp, &sid, "please !edit it").await;
    assert_eq!(r["stopReason"], "end_turn", "{format:?}");
    assert_eq!(acp.asked.len(), 1, "{format:?}");
    assert_eq!(acp.asked[0]["toolCall"]["kind"], "edit");
    assert_eq!(acp.asked[0]["toolCall"]["content"][0]["type"], "diff");
    assert_eq!(std::fs::read_to_string(cwd.join("patchbay-test.txt")).unwrap(), "hello from patchbay\n");
    assert!(acp.text_of("agent_message_chunk").contains("Tool finished"), "{format:?}: {}", acp.text_of("agent_message_chunk"));
    assert!(r["usage"]["outputTokens"].as_u64().unwrap() > 0);

    // A denied edit leaves the file alone.
    std::fs::remove_file(cwd.join("patchbay-test.txt")).unwrap();
    acp.answer = "reject";
    let r = prompt(&mut acp, &sid, "again !edit").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(!cwd.join("patchbay-test.txt").exists(), "{format:?}");
    let failed = acp.updates.iter().any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed");
    assert!(failed, "{format:?}");

    // The shell runs, streams and reports.
    acp.answer = "allow";
    let r = prompt(&mut acp, &sid, "now !run it").await;
    assert_eq!(r["stopReason"], "end_turn");
    let streamed: String = acp.updates.iter().filter_map(|u| u["_meta"]["terminal_output_delta"]["data"].as_str()).collect();
    assert!(streamed.contains("patchbay-ok"), "{format:?}: {streamed}");

    // The session reloads: history replays and the agent still answers.
    let mut again = Acp::start(format, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert!(again.text_of("user_message_chunk").contains("please !edit it"), "{format:?}");
    assert!(again.updates.iter().any(|u| u["sessionUpdate"] == "tool_call" && u["status"] == "completed"));
    let r = prompt(&mut again, &sid, "!tools").await;
    assert_eq!(r["stopReason"], "end_turn");
    let names = again.text_of("agent_message_chunk");
    assert!(names.contains("Bash") && names.contains("Edit") && names.contains("Grep"), "{format:?}: {names}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn chat_completions_edit_flow() {
    edit_flow(WireFormat::ChatCompletions).await;
}

#[tokio::test]
#[ignore]
async fn responses_edit_flow() {
    edit_flow(WireFormat::Responses).await;
}

#[tokio::test]
#[ignore]
async fn anthropic_edit_flow() {
    edit_flow(WireFormat::Anthropic).await;
}

#[tokio::test]
#[ignore]
async fn gemini_edit_flow() {
    edit_flow(WireFormat::Gemini).await;
}

#[tokio::test]
#[ignore]
async fn plan_mode_hides_writes_and_asks_to_exit() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    acp.call("session/set_mode", json!({"sessionId": sid, "modeId": "plan"})).await;
    let r = prompt(&mut acp, &sid, "!tools").await;
    assert_eq!(r["stopReason"], "end_turn");
    let names = acp.text_of("agent_message_chunk");
    let list: Vec<&str> = names.trim_start_matches("tool names: ").split(", ").collect();
    assert!(list.contains(&"ExitPlanMode") && list.contains(&"Grep"), "{names}");
    assert!(!list.contains(&"Bash") && !list.contains(&"Edit") && !list.contains(&"Write"), "{names}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn compaction_replaces_the_history_and_survives_a_reload() {
    let cwd = workdir();
    // A tiny window makes the first reply's usage cross the threshold.
    let mut acp = Acp::start_with(WireFormat::ChatCompletions, cwd.join("data"), Some(1000), Some(1));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "!run it").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.text_of("agent_thought_chunk").contains("Summarized the conversation"), "{}", acp.text_of("agent_thought_chunk"));

    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("\"t\":\"compacted\"") && store.contains("<conversation-summary>"), "{store}");

    let mut again = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert!(again.text_of("agent_message_chunk").contains("Earlier conversation summarized"), "{}", again.text_of("agent_message_chunk"));
    // The agent also compacts on request.
    let r = prompt(&mut again, &sid, "/compact keep the file names").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(again.text_of("agent_thought_chunk").contains("Summarized the conversation"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn rewind_drops_prompts_and_restores_files() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    prompt(&mut acp, &sid, "first !edit").await;
    assert!(cwd.join("patchbay-test.txt").exists());
    prompt(&mut acp, &sid, "second question").await;
    let points = acp.call("patchbay/rewind/points", json!({"sessionId": sid})).await;
    let points = points["rewind_points"].as_array().unwrap().clone();
    assert_eq!(points.len(), 2, "{points:?}");
    assert!(points[0]["prompt_preview"].as_str().unwrap().starts_with("first"));

    // Conversation only: the file stays.
    acp.call("patchbay/rewind/execute", json!({"sessionId": sid, "targetPromptIndex": 1, "mode": "conversation_only"})).await;
    assert!(cwd.join("patchbay-test.txt").exists());
    // Everything: the file the first prompt created goes away.
    acp.call("patchbay/rewind/execute", json!({"sessionId": sid, "targetPromptIndex": 0, "mode": "all"})).await;
    assert!(!cwd.join("patchbay-test.txt").exists());

    let mut again = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert_eq!(again.text_of("user_message_chunk"), "", "the rewound prompts must not replay");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn tells_the_model_about_files_changed_behind_its_back() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    prompt(&mut acp, &sid, "make it !edit").await;
    std::fs::write(cwd.join("patchbay-test.txt"), "someone else wrote this\n").unwrap();
    prompt(&mut acp, &sid, "carry on").await;
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("changed since you read it"), "{store}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn asks_the_user_a_question() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Responses, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    acp.pick = Some("opt-1");
    let r = prompt(&mut acp, &sid, "please !ask").await;
    assert_eq!(r["stopReason"], "end_turn");
    let q = &acp.asked[0];
    let ids: Vec<&str> = q["options"].as_array().unwrap().iter().filter_map(|o| o["optionId"].as_str()).collect();
    assert_eq!(ids, ["opt-0", "opt-1", "__ask_question_skip__"]);
    assert!(acp.text_of("agent_message_chunk").contains("= \"Beta\""), "{}", acp.text_of("agent_message_chunk"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_subagent_reports_back() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "please !task").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.text_of("agent_message_chunk").contains("SUBAGENT-OK"), "{}", acp.text_of("agent_message_chunk"));
    // Its tokens count toward the turn: more than the two main requests alone (59 each).
    assert!(r["usage"]["outputTokens"].as_u64().unwrap() >= 51, "{r}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn searches_the_web_through_brave() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    // The mock serves Brave's API on the same host as the model.
    unsafe { std::env::set_var("PATCHBAY_BRAVE_URL", std::env::var("PATCHBAY_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8765/v1".into()).trim_end_matches("/v1")) };
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "look it up !search").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert_eq!(acp.asked[0]["toolCall"]["kind"], "fetch");
    assert!(acp.text_of("agent_message_chunk").contains("BRAVE-MOCK-HIT & more"), "{}", acp.text_of("agent_message_chunk"));
    let _ = std::fs::remove_dir_all(cwd);
}
