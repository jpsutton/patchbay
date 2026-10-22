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
        Ok(ModelClient { client: Client::new(cfg), system_prefix: Vec::new(), context_window: Some(100_000) })
    }

    fn data_dir(&self) -> PathBuf {
        self.data.clone()
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
}

impl Acp {
    fn start(format: WireFormat, data: PathBuf) -> Acp {
        let (c_to_a, a_from_c) = tokio::io::duplex(1 << 20);
        let (a_to_c, c_from_a) = tokio::io::duplex(1 << 20);
        let host = Arc::new(TestHost { format, data });
        tokio::spawn(patchbay_agent::serve(host, a_from_c, a_to_c));
        Acp { to: c_to_a, from: BufReader::new(c_from_a).lines(), next: 1, updates: Vec::new(), asked: Vec::new(), answer: "allow" }
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
                    let option = if self.answer == "allow" { "allow" } else { "reject" };
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
