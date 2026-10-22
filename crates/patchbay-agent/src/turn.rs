//! One prompt: stream a reply, run the tools it asks for, repeat until it stops asking.

use std::sync::Arc;

use patchbay_llm::{Event, Message, Part, Request, Role, StopReason, ToolCall, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::prompt;
use crate::session::{Mode, PromptRec, Session, Totals, stamp_of};
use crate::store::Entry;
use crate::tools::{Registry, Tool, ToolCtx, ToolOutput, clip_middle};

/// Most model requests one prompt may make.
const MAX_STEPS: usize = 100;
/// Longest tool result text kept in the history.
const MAX_RESULT_CHARS: usize = 100_000;
/// Longest tool result text shown on the card.
const MAX_CARD_CHARS: usize = 20_000;

pub struct TurnResult {
    pub stop_reason: &'static str,
    pub usage: Totals,
}

/// ACP prompt content blocks as message parts.
pub fn blocks_to_parts(blocks: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => {
                if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                    parts.push(Part::Text(t.to_owned()));
                }
            }
            Some("image") => {
                if let (Some(data), Some(mime)) = (b["data"].as_str(), b["mimeType"].as_str()) {
                    parts.push(Part::Image { mime: mime.to_owned(), data: data.to_owned() });
                }
            }
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("attachment");
                let name = uri.rsplit('/').next().unwrap_or(uri);
                match r["text"].as_str() {
                    Some(text) => parts.push(Part::Text(format!(
                        "<file name=\"{name}\" type=\"{}\">\n{text}\n</file>",
                        r["mimeType"].as_str().unwrap_or("text/plain")
                    ))),
                    None => parts.push(Part::Text(format!("[Attached file: {uri}]"))),
                }
            }
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or("");
                parts.push(Part::Text(format!("[Attached file: {}]", uri.strip_prefix("file://").unwrap_or(uri))));
            }
            _ => {}
        }
    }
    parts
}

fn card_text(text: &str) -> String {
    clip_middle(text, MAX_CARD_CHARS)
}

/// What the harness wants the model to know that the user did not say: files that changed under
/// it, a model switch, a stale task list. `None` when there is nothing to add.
async fn reminders(session: &Session, model_name: &str) -> Option<Part> {
    let mut notes: Vec<String> = Vec::new();
    {
        let files = session.files.lock().unwrap().clone();
        let mut notified = session.notified.lock().unwrap();
        let mut paths: Vec<_> = files.keys().cloned().collect();
        paths.sort();
        for path in paths {
            let now = stamp_of(&path);
            if now == files.get(&path).copied() {
                continue;
            }
            if let Some(n) = now
                && notified.get(&path) == Some(&n)
            {
                continue;
            }
            match now {
                Some(n) => {
                    notified.insert(path.clone(), n);
                    notes.push(format!(
                        "{} changed since you read it (the user or a tool edited it). Read it again before you edit it.",
                        path.display()
                    ));
                }
                None => {
                    notified.remove(&path);
                    session.files.lock().unwrap().remove(&path);
                    notes.push(format!("{} was deleted since you read it.", path.display()));
                }
            }
        }
    }
    let mut st = session.state.lock().await;
    if let Some(prev) = &st.last_model
        && prev != &st.model_id
    {
        notes.push(format!("The session's model was changed to {model_name}. Earlier replies came from a different model."));
    }
    st.last_model = Some(st.model_id.clone());
    st.since_todo_write += 1;
    if st.since_todo_write >= 8 && st.todos.iter().any(|t| t.status != "completed") {
        st.since_todo_write = 0;
        let list: Vec<String> = st.todos.iter().map(|t| format!("- [{}] {}", t.status, t.content)).collect();
        notes.push(format!(
            "You have not used TodoWrite for a while. If the list below is out of date, update it; if the work is done, clear it. Current list:\n{}",
            list.join("\n")
        ));
    }
    (!notes.is_empty()).then(|| Part::Text(prompt::reminder(&notes.join("\n\n"))))
}

/// `/compact [focus]`: the user asked for a summary instead of a reply.
fn compact_command(parts: &[Part]) -> Option<String> {
    let [Part::Text(t)] = parts else { return None };
    let rest = t.trim().strip_prefix("/compact")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim().to_owned())
}

pub async fn run_turn(session: Arc<Session>, prompt_blocks: &Value, cancel: CancellationToken) -> Result<TurnResult, String> {
    let mut result = run_turn_inner(&session, prompt_blocks, cancel).await;
    if let Ok(r) = &mut result {
        let mut extra = session.extra_usage.lock().unwrap();
        r.usage.input += extra.input;
        r.usage.cached_read += extra.cached_read;
        r.usage.cache_write += extra.cache_write;
        r.usage.output += extra.output;
        r.usage.reasoning += extra.reasoning;
        *extra = Totals::default();
    }
    result
}

async fn run_turn_inner(session: &Arc<Session>, prompt_blocks: &Value, cancel: CancellationToken) -> Result<TurnResult, String> {
    let session = session.clone();
    let mut parts = blocks_to_parts(prompt_blocks);
    if parts.is_empty() {
        return Err("the prompt is empty".into());
    }
    if let Some(focus) = compact_command(&parts) {
        let model_id = session.state.lock().await.model_id.clone();
        let mc = session.host.model_client(&model_id).await?;
        return match compact::compact(&session, &mc, Some(&focus), &cancel).await {
            Ok(()) => Ok(TurnResult { stop_reason: "end_turn", usage: Totals::default() }),
            Err(e) if e == "cancelled" => Ok(TurnResult { stop_reason: "cancelled", usage: Totals::default() }),
            Err(e) => {
                session.text_chunk("agent_message_chunk", &format!("Could not compact the conversation: {e}"));
                Ok(TurnResult { stop_reason: "end_turn", usage: Totals::default() })
            }
        };
    }
    let model_name = {
        let id = session.state.lock().await.model_id.clone();
        session.host.models().into_iter().find(|m| m.id == id).map(|m| m.name).unwrap_or(id)
    };
    if let Some(r) = reminders(&session, &model_name).await {
        parts.push(r);
    }
    let preview: String = parts
        .iter()
        .find_map(|p| if let Part::Text(t) = p { Some(t.trim().chars().take(80).collect()) } else { None })
        .unwrap_or_default();
    let user = Message::with_parts(Role::User, parts);
    session.store.append(&Entry::Msg { m: user.clone() });
    {
        let mut st = session.state.lock().await;
        session.prompts.lock().unwrap().push(PromptRec {
            msg_index: st.messages.len(),
            preview,
            files: Default::default(),
        });
        st.messages.push(user);
    }

    let mut totals = Totals::default();
    for step in 0..MAX_STEPS {
        if cancel.is_cancelled() {
            return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
        }
        let (model_id, mode) = {
            let st = session.state.lock().await;
            (st.model_id.clone(), st.mode)
        };
        let settings = session.host.settings();
        let mc = session.host.model_client(&model_id).await?;
        let percent = mc
            .auto_compact_percent
            .or((settings.auto_compact_percent > 0).then_some(settings.auto_compact_percent))
            .unwrap_or(compact::DEFAULT_PERCENT);
        let context = session.state.lock().await.context_tokens;
        if compact::needs(context, mc.context_window, percent)
            && let Err(e) = compact::compact(&session, &mc, None, &cancel).await
        {
            if e == "cancelled" {
                return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
            }
            session.text_chunk("agent_thought_chunk", &format!("Could not compact the conversation: {e}\n"));
        }
        let messages = session.state.lock().await.messages.clone();
        let mcp = session.mcp.lock().await.clone();
        let mut registry = Registry::builtin(settings.web_fetch);
        for t in mcp.tools() {
            registry.push(t);
        }
        let mut system = mc.system_prefix.clone();
        let model_name = session.host.models().into_iter().find(|m| m.id == model_id).map(|m| m.name).unwrap_or(model_id.clone());
        system.extend(prompt::system_blocks(&session.cwd, &model_name, &settings.instructions, mode));
        let request = Request {
            system,
            messages,
            tools: registry.specs(mode == Mode::Plan),
            cache_key: Some(session.id.clone()),
            ..Default::default()
        };

        let mut streamed = String::new();
        let result = mc
            .client
            .complete(&request, &cancel, |ev| match ev {
                Event::Text(t) => {
                    streamed.push_str(&t);
                    session.text_chunk("agent_message_chunk", &t);
                }
                Event::Reasoning(t) => session.text_chunk("agent_thought_chunk", &t),
                Event::Retrying { attempt, max, reason } => {
                    session.text_chunk("agent_thought_chunk", &format!("Retrying ({attempt}/{max}): {reason}\n"))
                }
                Event::ToolCall(_) => {}
            })
            .await;
        let response = match result {
            Ok(r) => r,
            Err(e) if e.cancelled => {
                // Keep what the user saw, so the next prompt continues from it.
                if !streamed.is_empty() {
                    let m = Message::assistant(streamed);
                    session.store.append(&Entry::Msg { m: m.clone() });
                    session.state.lock().await.messages.push(m);
                }
                return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
            }
            Err(e) => return Err(e.to_string()),
        };

        if let Some(u) = response.usage {
            totals.input += u.input;
            totals.cached_read += u.cached_read;
            totals.cache_write += u.cache_write;
            totals.output += u.output;
            totals.reasoning += u.reasoning;
            session.state.lock().await.context_tokens = u.input + u.output;
            session.update(json!({
                "sessionUpdate": "usage_update",
                "used": u.input + u.output,
                "size": mc.context_window.unwrap_or(0),
                "_meta": {"autoCompactPercent": mc.auto_compact_percent.or((settings.auto_compact_percent > 0).then_some(settings.auto_compact_percent))},
            }));
        }
        let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        if !response.message.parts.is_empty() {
            session.store.append(&Entry::Msg { m: response.message.clone() });
            session.state.lock().await.messages.push(response.message);
        }
        if calls.is_empty() {
            let stop_reason = match response.stop {
                StopReason::MaxTokens => "max_tokens",
                StopReason::Refusal => "refusal",
                _ => "end_turn",
            };
            return Ok(TurnResult { stop_reason, usage: totals });
        }

        let results = execute(&session, &registry, calls, &cancel, "").await;
        let mut parts: Vec<Part> = results.into_iter().map(Part::ToolResult).collect();
        if let Some(r) = reminders(&session, &model_name).await {
            parts.push(r);
        }
        let msg = Message::with_parts(Role::User, parts);
        session.store.append(&Entry::Msg { m: msg.clone() });
        session.state.lock().await.messages.push(msg);
        if cancel.is_cancelled() {
            return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
        }
        if step + 1 == MAX_STEPS {
            return Ok(TurnResult { stop_reason: "max_turn_requests", usage: totals });
        }
    }
    Ok(TurnResult { stop_reason: "max_turn_requests", usage: totals })
}

/// Everything the card and the permission request show about a call.
fn card(tool: Option<&Arc<dyn Tool>>, call: &ToolCall, session: &Session) -> Value {
    let (title, kind, locations) = match tool {
        Some(t) => (
            t.title(&call.arguments),
            t.kind(),
            t.locations(&call.arguments, session),
        ),
        None => (call.name.clone(), "other", Vec::new()),
    };
    json!({
        "toolCallId": call.id,
        "title": title,
        "kind": kind,
        "status": "pending",
        "rawInput": call.arguments,
        "locations": locations.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
    })
}

/// Run a step's tool calls. `title_prefix` marks calls a subagent makes.
pub(crate) async fn execute(
    session: &Arc<Session>,
    registry: &Registry,
    calls: Vec<ToolCall>,
    cancel: &CancellationToken,
    title_prefix: &str,
) -> Vec<ToolResult> {
    let mode = session.state.lock().await.mode;
    let tools: Vec<Option<Arc<dyn Tool>>> = calls.iter().map(|c| registry.get(&c.name)).collect();
    // Show every call right away; they may then wait their turn for permission.
    let cards: Vec<Value> = calls
        .iter()
        .zip(&tools)
        .map(|(c, t)| {
            let mut card = card(t.as_ref(), c, session);
            if !title_prefix.is_empty() {
                card["title"] = json!(format!("{title_prefix}{}", card["title"].as_str().unwrap_or("")));
            }
            card
        })
        .collect();
    for c in &cards {
        let mut u = c.clone();
        u["sessionUpdate"] = json!("tool_call");
        session.update(u);
    }
    let mut results = Vec::with_capacity(calls.len());
    let mut i = 0;
    while i < calls.len() {
        // Consecutive read-only calls that need no approval run side by side.
        let parallel = |k: usize| {
            tools[k].as_ref().is_some_and(|t| t.read_only() && !t.needs_permission(&calls[k].arguments, session))
        };
        let mut j = i + 1;
        if parallel(i) {
            while j < calls.len() && parallel(j) {
                j += 1;
            }
        }
        let batch = futures::future::join_all((i..j).map(|k| {
            run_call(session.clone(), tools[k].clone(), calls[k].clone(), cards[k].clone(), mode, cancel.clone())
        }))
        .await;
        results.extend(batch);
        i = j;
    }
    results
}

enum Verdict {
    Allow,
    Reject,
    Cancelled,
}

async fn ask(session: &Session, card: &Value, tool: &Arc<dyn Tool>, call: &ToolCall, cancel: &CancellationToken) -> Verdict {
    let options = if tool.kind() == "switch_mode" {
        json!([
            {"optionId": "exit-plan-accept", "name": "Yes, start coding", "kind": "allow_once"},
            {"optionId": "exit-plan-keep", "name": "No, keep planning", "kind": "reject_once"},
        ])
    } else {
        json!([
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "reject", "name": "Deny", "kind": "reject_once"},
        ])
    };
    let mut tool_call = card.clone();
    let preview = tool.preview(&call.arguments, session).await;
    if !preview.is_empty() {
        tool_call["content"] = json!(preview);
    }
    let req = session.peer.request(
        "session/request_permission",
        json!({"sessionId": session.id, "toolCall": tool_call, "options": options}),
    );
    let reply = tokio::select! {
        _ = cancel.cancelled() => return Verdict::Cancelled,
        r = req => r,
    };
    match reply {
        Ok(v) if v["outcome"]["outcome"] == "selected" => match v["outcome"]["optionId"].as_str() {
            Some("allow" | "exit-plan-accept") => Verdict::Allow,
            _ => Verdict::Reject,
        },
        Ok(_) => Verdict::Cancelled,
        Err(_) => Verdict::Cancelled,
    }
}

fn finish(session: &Session, card: &Value, status: &str, content: Vec<Value>, raw: Value, meta: Option<Value>) {
    let mut update = card.clone();
    update["status"] = json!(status);
    update["content"] = json!(content);
    update["rawOutput"] = raw;
    // The stored copy is the whole card; the live update carries the terminal channel's end too.
    let mut stored = update.clone();
    stored["sessionUpdate"] = json!("tool_call");
    session.store.append(&Entry::Tool { call: stored });
    update["sessionUpdate"] = json!("tool_call_update");
    if let Some(m) = meta {
        update["_meta"] = m;
    }
    session.update(update);
}

fn text_content(text: &str) -> Vec<Value> {
    vec![json!({"type": "content", "content": {"type": "text", "text": card_text(text)}})]
}

async fn run_call(
    session: Arc<Session>,
    tool: Option<Arc<dyn Tool>>,
    call: ToolCall,
    card: Value,
    mode: Mode,
    cancel: CancellationToken,
) -> ToolResult {
    let fail = |session: &Session, text: String| {
        finish(session, &card, "failed", text_content(&text), json!(text), None);
        ToolResult::text(&call, text, true)
    };
    let Some(tool) = tool else {
        return fail(&session, format!("Unknown tool: {}", call.name));
    };
    if mode == Mode::Plan && !tool.allowed_in_plan() {
        return fail(
            &session,
            format!("{} is not available in plan mode. Explore with Read, Glob and Grep, then call ExitPlanMode with your plan.", call.name),
        );
    }
    if tool.needs_permission(&call.arguments, &session) {
        match ask(&session, &card, &tool, &call, &cancel).await {
            Verdict::Allow => {}
            Verdict::Reject => {
                return fail(
                    &session,
                    "The user declined this tool call, so it did not run. Do not retry it or work around it; ask what they would like instead."
                        .into(),
                );
            }
            Verdict::Cancelled => return fail(&session, "The tool call was cancelled before it ran.".into()),
        }
    }
    session.update(json!({"sessionUpdate": "tool_call_update", "toolCallId": call.id, "status": "in_progress"}));
    let ctx = ToolCtx { session: session.clone(), call_id: call.id.clone(), cancel: cancel.clone() };
    let out: ToolOutput = tokio::select! {
        out = tool.run(call.arguments.clone(), &ctx) => out,
        _ = cancel.cancelled() => ToolOutput::error("Interrupted by the user."),
    };
    let text = out.text_of();
    let content = if out.ui.is_empty() { text_content(&text) } else { out.ui.clone() };
    let raw = if out.raw.is_null() { json!(card_text(&text)) } else { out.raw.clone() };
    finish(&session, &card, if out.is_error { "failed" } else { "completed" }, content, raw, out.meta.clone());
    let content = out
        .content
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => Part::Text(clip_middle(&t, MAX_RESULT_CHARS)),
            other => other,
        })
        .collect();
    ToolResult { call_id: call.id, name: call.name, content, is_error: out.is_error }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_blocks_become_parts() {
        let blocks = json!([
            {"type": "text", "text": "look"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///x/a.csv", "mimeType": "text/csv", "text": "a,b"}},
            {"type": "resource_link", "uri": "file:///x/b.pdf", "name": "b.pdf"},
        ]);
        let parts = blocks_to_parts(&blocks);
        assert_eq!(parts.len(), 4);
        assert!(matches!(&parts[2], Part::Text(t) if t.contains("<file name=\"a.csv\" type=\"text/csv\">") && t.contains("a,b")));
        assert!(matches!(&parts[3], Part::Text(t) if t == "[Attached file: /x/b.pdf]"));
    }
}
