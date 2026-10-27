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
        vec![ModelInfo { id: "pb-mock".into(), name: "Mock".into(), effort: None }]
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
    /// Steer "STEERED-IN" into this session when the next permission request arrives.
    steer_on_ask: Option<String>,
}

impl Acp {
    fn start(format: WireFormat, data: PathBuf) -> Acp {
        Acp::start_with(format, data, Some(100_000), None)
    }

    fn start_with(format: WireFormat, data: PathBuf, window: Option<u64>, percent: Option<u8>) -> Acp {
        claude_home();
        let (c_to_a, a_from_c) = tokio::io::duplex(1 << 20);
        let (a_to_c, c_from_a) = tokio::io::duplex(1 << 20);
        let host = Arc::new(TestHost { format, data, window, percent });
        tokio::spawn(patchbay_agent::serve(host, a_from_c, a_to_c));
        Acp { to: c_to_a, from: BufReader::new(c_from_a).lines(), next: 1, updates: Vec::new(), asked: Vec::new(), answer: "allow", pick: None, steer_on_ask: None }
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
                    if let Some(sid) = self.steer_on_ask.take() {
                        let id = self.next;
                        self.next += 1;
                        let p = json!({"sessionId": sid, "prompt": [{"type": "text", "text": "STEERED-IN"}]});
                        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "session/steering", "params": p})).await;
                        // Let the agent take it before the turn moves on.
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    let option = self.pick.unwrap_or(if self.answer == "allow" { "allow" } else { "reject" });
                    self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": option}}})).await;
                }
                _ => {}
            }
        }
    }

    /// Like `call`, for a request that must fail: the error's message.
    async fn call_err(&mut self, method: &str, params: Value) -> String {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let line = self.from.next_line().await.unwrap().expect("agent closed");
            let m: Value = serde_json::from_str(&line).unwrap();
            if m["id"] == json!(id) && m.get("method").is_none() {
                return m["error"]["message"].as_str().unwrap_or("").to_owned();
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
    let init = acp
        .call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {"_meta": {"subagents": true}}}))
        .await;
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
async fn keepwarm_refreshes_the_cache_and_toggles_auto() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;

    // Nothing to refresh before the first prompt.
    let r = prompt(&mut acp, &sid, "/keepwarm").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.text_of("agent_message_chunk").contains("Nothing is cached yet"), "{}", acp.text_of("agent_message_chunk"));

    // A reply that read the cache starts the clock, and the client is told.
    prompt(&mut acp, &sid, "hello !cached").await;
    let clock = |acp: &Acp| acp.updates.iter().rev().find_map(|u| u["_meta"]["cache"].as_object().cloned());
    let c = clock(&acp).expect("a usage_update carries the cache clock");
    assert_eq!(c["ttlSecs"], 300);
    assert_eq!(c["auto"], false);
    assert!(c["lastAt"].as_u64().is_some_and(|t| t > 0), "{c:?}");

    // The refresh reads the cache again, answers the user, and leaves the history alone.
    let before = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    let r = prompt(&mut acp, &sid, "/keepwarm").await;
    assert_eq!(r["stopReason"], "end_turn");
    let said = acp.text_of("agent_message_chunk");
    assert!(said.contains("Cache refreshed: 4k tokens read from cache, good for another 5m."), "{said}");
    let after = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(!after.replace(&before, "").contains("\"t\":\"msg\""), "the refresh wrote to the history");

    // Auto mode toggles and shows in the clock.
    prompt(&mut acp, &sid, "/keepwarm auto").await;
    assert!(acp.text_of("agent_message_chunk").contains("Auto keep-warm on"));
    assert_eq!(clock(&acp).unwrap()["auto"], true);
    prompt(&mut acp, &sid, "/keepwarm auto").await;
    assert!(acp.text_of("agent_message_chunk").contains("Auto keep-warm off."));
    assert_eq!(clock(&acp).unwrap()["auto"], false);

    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn keepwarm_says_when_the_model_has_no_refreshable_cache() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    prompt(&mut acp, &sid, "hello").await;
    prompt(&mut acp, &sid, "/keepwarm").await;
    assert!(acp.text_of("agent_message_chunk").contains("only be refreshed on Anthropic models"), "{}", acp.text_of("agent_message_chunk"));
    assert!(acp.updates.iter().all(|u| u["_meta"]["cache"].is_null()), "no clock for a format without a known TTL");
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
async fn a_tool_call_cut_off_by_the_output_limit_does_not_run() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "write it !cutcall").await;
    // The model hears why the call failed and the turn carries on.
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.asked.is_empty(), "a cut-off call must not ask for permission");
    assert!(!cwd.join("cut-off.txt").exists());
    let failed = acp.updates.iter().find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed").expect("failed card");
    assert!(failed["rawOutput"].as_str().unwrap().contains("output token limit"), "{failed}");
    assert!(acp.text_of("agent_message_chunk").contains("Tool finished"), "{}", acp.text_of("agent_message_chunk"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_reply_cut_off_by_the_output_limit_is_continued_a_few_times() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;

    // Cut off once: the agent asks for the rest and the turn ends normally.
    let r = prompt(&mut acp, &sid, "explain !cutreply").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.text_of("agent_thought_chunk").contains("continuing (1/3)"));
    assert!(acp.text_of("agent_message_chunk").contains("Echo:"), "{}", acp.text_of("agent_message_chunk"));
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("reached the output token limit and was cut off"), "{store}");

    // Cut off every time: it gives up after three tries and says why.
    acp.updates.clear();
    let r = prompt(&mut acp, &sid, "explain !cutalways").await;
    assert_eq!(r["stopReason"], "max_tokens");
    let thoughts = acp.text_of("agent_thought_chunk");
    assert!(thoughts.contains("continuing (3/3)") && !thoughts.contains("(4/3)"), "{thoughts}");
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

#[tokio::test]
#[ignore]
async fn subagents_run_side_by_side_and_report_back() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "search twice !par").await;
    assert_eq!(r["stopReason"], "end_turn");
    let text = acp.text_of("agent_message_chunk");
    assert!(text.contains("PAR-A") && text.contains("PAR-B"), "{text}");
    // Their sub-calls would carry a prefixed id; here both Task cards finished.
    let done = acp.updates.iter().filter(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed").count();
    assert!(done >= 2, "{done}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_background_subagent_keeps_the_prompt_open_until_it_reports() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "start one !bg").await;
    assert_eq!(r["stopReason"], "end_turn");
    // The main agent got the notification and answered it within the same prompt.
    let text = acp.text_of("agent_message_chunk");
    assert!(text.contains("BG-DONE"), "{text}");
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("<task-notification>") && store.contains("<agent-id>scout</agent-id>"), "{store}");
    // On replay the notification is not shown as something the user said.
    let mut again = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert!(!again.text_of("user_message_chunk").contains("task-notification"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_background_subagent_waits_for_the_next_prompt_without_the_capability() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    acp.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let s = acp.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
    let sid = s["sessionId"].as_str().unwrap().to_owned();
    let r = prompt(&mut acp, &sid, "start one !bg").await;
    assert_eq!(r["stopReason"], "end_turn");
    // Give the subagent a moment, then the next prompt carries its report.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    prompt(&mut acp, &sid, "what did it say?").await;
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("BG-DONE") && store.contains("<task-notification>"), "{store}");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_fork_starts_from_the_conversation() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Responses, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "branch off !fork").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(acp.text_of("agent_message_chunk").contains("FORK-OK"), "{}", acp.text_of("agent_message_chunk"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_named_subagent_can_be_continued() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Gemini, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    prompt(&mut acp, &sid, "make one !named").await;
    assert!(acp.text_of("agent_message_chunk").contains("NAMED-OK"), "{}", acp.text_of("agent_message_chunk"));
    prompt(&mut acp, &sid, "tell it !msg").await;
    assert!(acp.text_of("agent_message_chunk").contains("MSG-OK"), "{}", acp.text_of("agent_message_chunk"));
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_worktree_subagent_edits_apart_from_the_checkout() {
    let cwd = workdir();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git").arg("-C").arg(&cwd).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(cwd.join("README.md"), "hi").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = prompt(&mut acp, &sid, "isolated !wt").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(!cwd.join("patchbay-test.txt").exists(), "the edit must not land in the main checkout");
    assert!(acp.text_of("agent_message_chunk").contains("git worktree"), "{}", acp.text_of("agent_message_chunk"));
    // The subagent's edit asked first, with a card that has its own id.
    assert!(acp.asked.iter().any(|a| a["toolCall"]["toolCallId"].as_str().is_some_and(|id| id.starts_with("agent_1:"))), "{:?}", acp.asked);
    let worktrees = std::fs::read_dir(cwd.join("data/agent/worktrees")).unwrap().count();
    assert_eq!(worktrees, 1);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn prompts_can_be_minified_per_session_and_the_choice_is_kept() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    acp.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    // A client that does not ask gets the host's setting (off here), and is told about the option.
    let s = acp.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
    let sid = s["sessionId"].as_str().unwrap().to_owned();
    let opt = option(&s, "minify_prompts");
    assert_eq!((opt["id"].as_str(), opt["currentValue"].as_str()), (Some("minify_prompts"), Some("off")));
    assert_eq!(s["models"]["currentModelId"], "pb-mock", "legacy model list stays");

    // The tool list the model sees: minified descriptions are shorter. `!tools` only lists names,
    // so read the size the mock reports for the request instead.
    let before = request_size(&mut acp, &sid).await;
    let r = acp.call("session/set_config_option", json!({"sessionId": sid, "configId": "minify_prompts", "value": "on"})).await;
    assert_eq!(option(&r, "minify_prompts")["currentValue"], "on");
    let after = request_size(&mut acp, &sid).await;
    println!("request: {before} -> {after} bytes ({}% smaller)", 100 - after * 100 / before);
    // The terse-reply instruction is part of the minified request, so it saves less than the texts alone.
    assert!(after < before, "minified request {after} vs full {before} bytes");
    assert!(acp.call_err("session/set_config_option", json!({"sessionId": sid, "configId": "minify_prompts", "value": "maybe"})).await.contains("on or off"));
    assert!(acp.call_err("session/set_config_option", json!({"sessionId": sid, "configId": "nope", "value": "on"})).await.contains("unknown config option"));

    // With the option on, each prompt carries the terse reminder, and with it off it does not.
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert_eq!(store.matches("TERSE MODE ACTIVE").count(), 1, "only the prompt sent after switching on gets it");

    // The choice survives a reload; a caller can also ask for it when it opens a session.
    let mut again = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let loaded = again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert_eq!(option(&loaded, "minify_prompts")["currentValue"], "on");
    let fresh = again.call("session/new", json!({"cwd": cwd, "mcpServers": [], "_meta": {"minifyPrompts": true}})).await;
    assert_eq!(option(&fresh, "minify_prompts")["currentValue"], "on");
    let _ = std::fs::remove_dir_all(cwd);
}

/// Bytes of the request the model gets for a trivial prompt, as the mock reports them.
async fn request_size(acp: &mut Acp, sid: &str) -> usize {
    prompt(acp, sid, "!size").await;
    let text = acp.text_of("agent_message_chunk");
    let n = text.rsplit("request bytes: ").next().unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap();
    n.parse().unwrap_or_else(|_| panic!("no size in {text:?}"))
}

/// A config option from a `session/new`, `session/load` or `session/set_config_option` answer.
fn option<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["configOptions"].as_array().and_then(|a| a.iter().find(|o| o["id"] == id)).unwrap_or_else(|| panic!("no option {id} in {v}"))
}

#[tokio::test]
#[ignore]
async fn thinking_effort_is_a_session_option_and_is_kept() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let r = acp.call("session/set_config_option", json!({"sessionId": sid, "configId": "effort", "value": "high"})).await;
    let effort = option(&r, "effort");
    assert_eq!((effort["category"].as_str(), effort["currentValue"].as_str()), (Some("thought_level"), Some("high")));
    assert!(acp.call_err("session/set_config_option", json!({"sessionId": sid, "configId": "effort", "value": "huge"})).await.contains("unknown effort"));
    prompt(&mut acp, &sid, "hello").await;

    let mut again = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    again.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let loaded = again.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert_eq!(option(&loaded, "effort")["currentValue"], "high");
    let r = again.call("session/set_config_option", json!({"sessionId": sid, "configId": "effort", "value": "default"})).await;
    assert_eq!(option(&r, "effort")["currentValue"], "default");
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
#[ignore]
async fn a_message_steered_in_joins_the_running_turn() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::Anthropic, cwd.join("data"));
    let init = acp.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    assert_eq!(init["_meta"]["steering"]["supported"], true);
    let s = acp.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
    let sid = s["sessionId"].as_str().unwrap().to_owned();
    // No turn runs: the message is handed back to be sent as a prompt.
    let idle = acp.call("session/steering", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "early"}]})).await;
    assert_eq!(idle["outcome"], "promptRequired");
    // While the turn waits on a question, steer a message in; the turn's next request carries it.
    acp.pick = Some("opt-1");
    acp.steer_on_ask = Some(sid.clone());
    let r = prompt(&mut acp, &sid, "please !ask").await;
    assert_eq!(r["stopReason"], "end_turn");
    let store = std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store.contains("STEERED-IN") && store.contains("while you were working"), "{store}");
    let _ = std::fs::remove_dir_all(cwd);
}

/// A Claude Code setup of the tests' own, so they never load the real `~/.claude`: a command, a
/// skill, a subagent, and hooks that only act when a marker file sits in the session's directory.
fn claude_home() {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("pb-e2e-claude-{}", std::process::id())).join(".claude");
        let put = |rel: &str, text: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put("commands/greet.md", "---\ndescription: Greet someone\nargument-hint: <name>\n---\nSay GREET-BODY to $ARGUMENTS");
        put("skills/pdf/SKILL.md", "---\nname: pdf\ndescription: Work with PDF files\n---\nUse pdftotext.");
        put("agents/scout.md", "---\nname: scout\ndescription: Looks around\ntools: Read, Grep\n---\nYou scout.");
        put(
            "settings.json",
            &json!({"hooks": {
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "grep -q hookme && echo HOOK-CONTEXT || true"}]}],
                "PreToolUse": [{"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "if [ -f .deny-edits ]; then echo NO-EDITS >&2; exit 2; fi"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "if [ -f .stop-once ]; then rm .stop-once; echo KEEP-GOING >&2; exit 2; fi"}]}],
            }})
            .to_string(),
        );
        // Tests run in one process; this is set before any agent starts.
        unsafe { std::env::set_var("PATCHBAY_CLAUDE_HOME", &home) };
        home
    });
}

#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn loads_the_claude_code_setup() {
    let cwd = workdir();
    let mut acp = Acp::start(WireFormat::ChatCompletions, cwd.join("data"));
    let sid = open(&mut acp, &cwd).await;
    let commands: Vec<String> = acp
        .updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "available_commands_update")
        .flat_map(|u| u["availableCommands"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["name"].as_str().map(str::to_owned))
        .collect();
    assert!(["compact", "greet", "pdf"].iter().all(|c| commands.iter().any(|x| x == c)), "{commands:?}");

    // A slash command expands beside what the user typed; a prompt hook adds context.
    prompt(&mut acp, &sid, "/greet Bob hookme").await;
    let store = || std::fs::read_to_string(cwd.join("data/agent/sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(store().contains("GREET-BODY to Bob hookme") && store().contains("HOOK-CONTEXT"), "{}", store());

    // The skill tool and the user's subagent type are offered.
    prompt(&mut acp, &sid, "!tools").await;
    assert!(acp.text_of("agent_message_chunk").contains("Skill"), "{}", acp.text_of("agent_message_chunk"));

    // A PreToolUse hook that exits 2 blocks the edit.
    std::fs::write(cwd.join(".deny-edits"), "").unwrap();
    prompt(&mut acp, &sid, "make it !edit").await;
    assert!(!cwd.join("patchbay-test.txt").exists());
    assert!(store().contains("NO-EDITS"), "{}", store());

    // A Stop hook sends the model back once.
    std::fs::write(cwd.join(".stop-once"), "").unwrap();
    let r = prompt(&mut acp, &sid, "hello").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert!(store().contains("KEEP-GOING") && !cwd.join(".stop-once").exists(), "{}", store());
    let _ = std::fs::remove_dir_all(cwd);
}
