//! Round trips against `scripts/mock-llm.py` (`scripts/mock-llm.py &`, then
//! `cargo test -p patchbay-llm -- --ignored`). Set PATCHBAY_MOCK_URL to move it.

use patchbay_llm::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn base() -> String {
    std::env::var("PATCHBAY_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8765/v1".into())
}

fn client(format: WireFormat) -> Client {
    let mut c = Config::new(format, base(), "mock-agent");
    c.key = Some("test-key".into());
    Client::new(c)
}

fn bash_tool() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "run a command".into(),
        parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
    }
}

async fn tool_round_trip(format: WireFormat) {
    let c = client(format);
    let cancel = CancellationToken::new();
    let mut req = Request {
        system: vec![SystemBlock::new("be terse")],
        messages: vec![Message::user("please !run it")],
        tools: vec![bash_tool()],
        ..Default::default()
    };
    let mut streamed = String::new();
    let first = c
        .complete(&req, &cancel, |e| {
            if let Event::Text(t) = e {
                streamed.push_str(&t)
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{format:?}: {e}"));
    assert_eq!(first.stop, StopReason::ToolUse, "{format:?}");
    let call = first.message.tool_calls().next().expect("tool call").clone();
    assert_eq!(call.name, "bash");
    assert!(call.arguments["command"].as_str().unwrap().contains("patchbay-ok"));
    assert!(first.usage.is_some(), "{format:?} usage");

    req.messages.push(first.message);
    req.messages.push(Message::with_parts(
        Role::User,
        vec![Part::ToolResult(ToolResult::text(&call, "patchbay-ok", false))],
    ));
    let second = c.complete(&req, &cancel, |_| {}).await.unwrap_or_else(|e| panic!("{format:?}: {e}"));
    assert_eq!(second.stop, StopReason::EndTurn, "{format:?}");
    assert!(second.message.text().contains("Tool finished"), "{format:?}: {}", second.message.text());
}

#[tokio::test]
#[ignore]
async fn chat_completions_tool_round_trip() {
    tool_round_trip(WireFormat::ChatCompletions).await;
}

#[tokio::test]
#[ignore]
async fn responses_tool_round_trip() {
    tool_round_trip(WireFormat::Responses).await;
}

#[tokio::test]
#[ignore]
async fn anthropic_tool_round_trip() {
    tool_round_trip(WireFormat::Anthropic).await;
}

#[tokio::test]
#[ignore]
async fn gemini_tool_round_trip() {
    // The Gemini path embeds the model id, so the mock serves it under the same base.
    tool_round_trip(WireFormat::Gemini).await;
}

#[tokio::test]
#[ignore]
async fn http_errors_surface_with_status() {
    let mut c = Config::new(WireFormat::Gemini, base(), "mock-agent");
    c.key = None;
    c.max_retries = 0;
    let e = Client::new(c)
        .complete_text(&Request::simple("s", "hi"), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.status, Some(403));
    assert!(e.message.contains("API key missing"), "{e}");
}
