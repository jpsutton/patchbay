//! One prompt: stream a reply, run the tools it asks for, repeat until it stops asking.

use std::sync::Arc;

use patchbay_llm::{Event, Message, Part, Request, Role, StopReason, ToolCall, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::claude;
use crate::compact;
use crate::prompt;
use crate::session::{Mode, PromptRec, Session, Totals, stamp_of};
use crate::subagent::notes_part;
use crate::store::Entry;
use crate::tools::{Registry, Tool, ToolCtx, ToolOutput, clip_middle};

/// Longest tool result text kept in the history.
const MAX_RESULT_CHARS: usize = 100_000;
/// Longest tool result text shown on the card.
const MAX_CARD_CHARS: usize = 20_000;
/// Replies in a row cut off by the output token limit that a turn picks up again before it stops.
pub(crate) const MAX_CUT_OFF_CONTINUES: usize = 3;

/// The call a reply was still writing when it hit the output token limit. Its arguments never
/// finished: they are cut-off JSON (kept as a string) or nothing at all. Running it would only
/// fail on missing fields, and the model would not learn why.
pub(crate) fn cut_off_call(calls: &[ToolCall], stop: &StopReason) -> Option<usize> {
    if *stop != StopReason::MaxTokens {
        return None;
    }
    let last = calls.len().checked_sub(1)?;
    let complete = calls[last].arguments.as_object().is_some_and(|o| !o.is_empty());
    (!complete).then_some(last)
}

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
    let p = session.prompts();
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
                    notes.push(p.msg("file_changed", &[("path", &path.display().to_string())]));
                }
                None => {
                    notified.remove(&path);
                    session.files.lock().unwrap().remove(&path);
                    notes.push(p.msg("file_deleted", &[("path", &path.display().to_string())]));
                }
            }
        }
    }
    let mut st = session.state.lock().await;
    if let Some(prev) = &st.last_model
        && prev != &st.model_id
    {
        notes.push(p.msg("model_changed", &[("model", model_name)]));
    }
    st.last_model = Some(st.model_id.clone());
    st.since_todo_write += 1;
    if st.since_todo_write >= 8 && st.todos.iter().any(|t| t.status != "completed") {
        st.since_todo_write = 0;
        let list: Vec<String> = st.todos.iter().map(|t| format!("- [{}] {}", t.status, t.content)).collect();
        notes.push(p.msg("todo_stale", &[("list", &list.join("\n"))]));
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
        let mc = session.model_client(&model_id).await?;
        return match compact::compact(&session, &mc, Some(&focus), &cancel).await {
            Ok(()) => Ok(TurnResult { stop_reason: "end_turn", usage: Totals::default() }),
            Err(e) if e == "cancelled" => Ok(TurnResult { stop_reason: "cancelled", usage: Totals::default() }),
            Err(e) => {
                session.text_chunk("agent_message_chunk", &format!("Could not compact the conversation: {e}"));
                Ok(TurnResult { stop_reason: "end_turn", usage: Totals::default() })
            }
        };
    }
    if session.reload_claude() {
        session.announce_commands();
    }
    let setup = session.claude();
    let mode_now = session.state.lock().await.mode;
    let typed: String = parts.iter().find_map(|p| if let Part::Text(t) = p { Some(t.clone()) } else { None }).unwrap_or_default();
    let mut context: Vec<String> = std::mem::take(&mut *session.start_context.lock().unwrap());
    if setup.has_hooks("UserPromptSubmit") {
        let mut input = session.hook_input(mode_now);
        input["prompt"] = json!(typed);
        let r = claude::run_hooks(&setup.hooks, "UserPromptSubmit", None, input, &session.cwd).await;
        session.hook_notes(&r.notes);
        if let Some(reason) = r.block.or(r.stop) {
            session.text_chunk("agent_message_chunk", &format!("A hook blocked this prompt: {reason}"));
            return Ok(TurnResult { stop_reason: "end_turn", usage: Totals::default() });
        }
        context.extend(r.context);
    }
    let p = session.prompts();
    // A slash command or skill the user ran: the text they typed stays as it is (and shows on
    // reload); its instructions go beside it.
    if let Some(expanded) = setup.expand_slash(&typed) {
        let name = typed.trim_start().trim_start_matches('/').split_whitespace().next().unwrap_or("");
        parts.push(Part::Text(prompt::reminder(&p.msg("slash_command", &[("name", name), ("text", &expanded)]))));
    }
    for c in context {
        parts.push(Part::Text(prompt::reminder(&p.msg("hook_context", &[("text", &c)]))));
    }
    let model_name = {
        let id = session.state.lock().await.model_id.clone();
        session.host.models().into_iter().find(|m| m.id == id).map(|m| m.name).unwrap_or(id)
    };
    if let Some(r) = reminders(&session, &model_name).await {
        parts.push(r);
    }
    parts.extend(notes_part(&session.agents.drain_notes(), session.prompts()));
    if let Some(nudge) = session.prompts().terse_nudge() {
        parts.push(Part::Text(prompt::reminder(nudge)));
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
    // Times a Stop hook kept this turn going; capped so a hook can't loop forever.
    let mut stop_rounds = 0;
    // Replies in a row cut off by the output token limit.
    let mut cut_offs = 0;
    let max_steps = session.host.settings().limits.max_steps.max(1);
    'steps: for step in 0..max_steps {
        if cancel.is_cancelled() {
            return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
        }
        let (model_id, mode) = {
            let st = session.state.lock().await;
            (st.model_id.clone(), st.mode)
        };
        let settings = session.host.settings();
        let mc = session.model_client(&model_id).await?;
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
        let mut registry = Registry::builtin(settings.web_fetch, session.host.web_search_key().is_some());
        for t in mcp.tools() {
            registry.push(t);
        }
        if !setup.skills.is_empty() {
            registry.push(Arc::new(crate::tools::skill::Skill));
        }
        if !setup.agents.is_empty() {
            let custom = setup.agents.iter().map(|a| (a.name.clone(), a.description.clone())).collect();
            registry.retain(|n| n != "Task");
            registry.push(Arc::new(crate::tools::task::Task { custom }));
        }
        let mut system = mc.system_prefix.clone();
        let model_name = session.host.models().into_iter().find(|m| m.id == model_id).map(|m| m.name).unwrap_or(model_id.clone());
        let extras = prompt::Extras { claude: settings.claude.clone(), skills: &setup.skills };
        system.extend(prompt::system_blocks(&session.cwd, &model_name, &settings.instructions, mode, session.prompts(), &extras));
        let request = Request {
            system,
            messages,
            tools: registry.specs(mode == Mode::Plan, session.prompts()),
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
            let mut update = json!({
                "sessionUpdate": "usage_update",
                "used": u.input + u.output,
                "size": mc.context_window.unwrap_or(0),
                "_meta": {"autoCompactPercent": mc.auto_compact_percent.or((settings.auto_compact_percent > 0).then_some(settings.auto_compact_percent))},
            });
            // Only an endpoint that reports cost (OpenRouter, say) has one; otherwise the host
            // prices the tokens from the model's settings.
            if let Some(cost) = session.add_cost(u.cost_usd) {
                update["cost"] = json!({"amount": cost, "currency": "USD"});
            }
            if let Some(plan) = &response.plan {
                update["_meta"]["_claude/rateLimit"] = plan_meta(plan);
            }
            session.update(update);
        }
        let cut = response.stop == StopReason::MaxTokens;
        cut_offs = if cut { cut_offs + 1 } else { 0 };
        let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        let cut_off = cut_off_call(&calls, &response.stop);
        if !response.message.parts.is_empty() {
            session.store.append(&Entry::Msg { m: response.message.clone() });
            session.state.lock().await.messages.push(response.message);
        }
        if calls.is_empty() {
            // The reply ran out of room mid-thought or mid-sentence: ask for the rest, a few times
            // at most.
            if cut && cut_offs <= MAX_CUT_OFF_CONTINUES {
                session.text_chunk(
                    "agent_thought_chunk",
                    &format!("\nHit the output token limit; continuing ({cut_offs}/{MAX_CUT_OFF_CONTINUES})\n"),
                );
                let note = session.prompts().msg("cut_off_reply", &[]);
                push_user(&session, vec![Part::Text(prompt::reminder(&note))]).await;
                continue 'steps;
            }
            let stop_reason = match response.stop {
                StopReason::MaxTokens => "max_tokens",
                StopReason::Refusal => "refusal",
                _ => "end_turn",
            };
            if stop_reason == "end_turn" {
                // Background subagents and messages steered in: read what has come in, and keep
                // the prompt open for the rest.
                loop {
                    let mut next: Vec<Part> = notes_part(&session.agents.drain_notes(), session.prompts()).into_iter().collect();
                    next.extend(session.take_steered(false));
                    if !next.is_empty() {
                        push_user(&session, next).await;
                        continue 'steps;
                    }
                    if session.agents.pending() > 0 && session.keep_open.load(std::sync::atomic::Ordering::SeqCst) {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                session.agents.stop_background();
                                return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
                            }
                            _ = session.agents.wait() => {}
                            _ = session.steered.notified() => {}
                        }
                        continue;
                    }
                    // Stop hooks may send the model back to work, a few times at most.
                    if setup.has_hooks("Stop") && stop_rounds < 3 {
                        let mut input = session.hook_input(mode);
                        input["stop_hook_active"] = json!(stop_rounds > 0);
                        let r = claude::run_hooks(&setup.hooks, "Stop", None, input, &session.cwd).await;
                        session.hook_notes(&r.notes);
                        if let Some(reason) = r.block {
                            stop_rounds += 1;
                            let note = session.prompts().msg("hook_stop", &[("reason", &reason)]);
                            push_user(&session, vec![Part::Text(prompt::reminder(&note))]).await;
                            continue 'steps;
                        }
                    }
                    // The last look: from here on a steered message is handed back as a prompt.
                    let last = session.take_steered(true);
                    if !last.is_empty() {
                        push_user(&session, last).await;
                        continue 'steps;
                    }
                    break;
                }
            }
            return Ok(TurnResult { stop_reason, usage: totals });
        }

        let results = execute(&session, &registry, calls, cut_off, &cancel, "").await;
        let mut parts: Vec<Part> = results.into_iter().map(Part::ToolResult).collect();
        if let Some(r) = reminders(&session, &model_name).await {
            parts.push(r);
        }
        parts.extend(notes_part(&session.agents.drain_notes(), session.prompts()));
        parts.extend(session.take_steered(false));
        push_user(&session, parts).await;
        if cancel.is_cancelled() {
            return Ok(TurnResult { stop_reason: "cancelled", usage: totals });
        }
        if step + 1 == max_steps {
            return Ok(TurnResult { stop_reason: "max_turn_requests", usage: totals });
        }
    }
    Ok(TurnResult { stop_reason: "max_turn_requests", usage: totals })
}

/// Claude subscription usage in the shape claude-agent-acp reports it (`_claude/rateLimit`), so
/// the host's plan-limit guard reads both agents alike.
fn plan_meta(plan: &patchbay_llm::PlanUsage) -> Value {
    let window = |w: Option<patchbay_llm::PlanWindow>| w.map(|w| json!({"utilization": w.utilization, "resetsAt": w.resets_at}));
    json!({
        "unifiedWindows": {"five_hour": window(plan.five_hour), "seven_day": window(plan.seven_day)},
        "isUsingOverage": plan.using_overage,
        "resetsAt": plan.resets_at,
    })
}

/// Add a user-role message to the history.
async fn push_user(session: &Session, parts: Vec<Part>) {
    let msg = Message::with_parts(Role::User, parts);
    session.store.append(&Entry::Msg { m: msg.clone() });
    session.state.lock().await.messages.push(msg);
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
        "toolCallId": format!("{}{}", session.card_prefix, call.id),
        "title": title,
        "kind": kind,
        "status": "pending",
        "rawInput": call.arguments,
        "locations": locations.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
    })
}

/// Run a step's tool calls. `cut_off` is the call the output token limit cut short (see
/// [`cut_off_call`]); it fails without running. `title_prefix` marks calls a subagent makes.
pub(crate) async fn execute(
    session: &Arc<Session>,
    registry: &Registry,
    calls: Vec<ToolCall>,
    cut_off: Option<usize>,
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
            tools[k].as_ref().is_some_and(|t| t.concurrent(&calls[k].arguments) && !t.needs_permission(&calls[k].arguments, session))
        };
        let mut j = i + 1;
        if parallel(i) {
            while j < calls.len() && parallel(j) {
                j += 1;
            }
        }
        let batch = futures::future::join_all((i..j).map(|k| {
            run_call(session.clone(), tools[k].clone(), calls[k].clone(), cards[k].clone(), cut_off == Some(k), mode, cancel.clone())
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
    cut_off: bool,
    mode: Mode,
    cancel: CancellationToken,
) -> ToolResult {
    let fail = |session: &Session, text: String| {
        finish(session, &card, "failed", text_content(&text), json!(text), None);
        ToolResult::text(&call, text, true)
    };
    if cut_off {
        return fail(&session, session.prompts().msg("cut_off_call", &[("name", &call.name)]));
    }
    let Some(tool) = tool else {
        return fail(&session, session.prompts().msg("unknown_tool", &[("name", &call.name)]));
    };
    if mode == Mode::Plan && !tool.allowed_in_plan() {
        return fail(&session, session.prompts().msg("not_in_plan_mode", &[("name", &call.name)]));
    }
    let setup = session.claude();
    // PreToolUse hooks may block the call, or decide the permission question themselves.
    let mut decided: Option<String> = None;
    if setup.has_hooks("PreToolUse") {
        let mut input = session.hook_input(mode);
        input["tool_name"] = json!(call.name);
        input["tool_input"] = call.arguments.clone();
        let r = claude::run_hooks(&setup.hooks, "PreToolUse", Some(&call.name), input, &session.cwd).await;
        session.hook_notes(&r.notes);
        let denied = if r.permission.as_deref() == Some("deny") { r.permission_reason.clone().or(Some("denied".into())) } else { None };
        if let Some(reason) = r.block.or(r.stop).or(denied) {
            return fail(&session, session.prompts().msg("hook_blocked_tool", &[("reason", &reason)]));
        }
        decided = r.permission;
    }
    let must_ask = match decided.as_deref() {
        Some("allow") => false,
        Some("ask") => true,
        _ => tool.needs_permission(&call.arguments, &session),
    };
    if must_ask {
        match ask(&session, &card, &tool, &call, &cancel).await {
            Verdict::Allow => {}
            Verdict::Reject => {
                return fail(&session, session.prompts().msg("declined", &[]));
            }
            Verdict::Cancelled => return fail(&session, session.prompts().msg("cancelled_before_run", &[])),
        }
    }
    let card_id = card["toolCallId"].as_str().unwrap_or(&call.id).to_owned();
    session.update(json!({"sessionUpdate": "tool_call_update", "toolCallId": card_id, "status": "in_progress"}));
    let ctx = ToolCtx { session: session.clone(), call_id: card_id, cancel: cancel.clone() };
    let out: ToolOutput = tokio::select! {
        out = tool.run(call.arguments.clone(), &ctx) => out,
        _ = cancel.cancelled() => ToolOutput::error(session.prompts().msg("interrupted", &[])),
    };
    let text = out.text_of();
    let content = if out.ui.is_empty() { text_content(&text) } else { out.ui.clone() };
    let raw = if out.raw.is_null() { json!(card_text(&text)) } else { out.raw.clone() };
    finish(&session, &card, if out.is_error { "failed" } else { "completed" }, content, raw, out.meta.clone());
    let mut out = out;
    if setup.has_hooks("PostToolUse") && !cancel.is_cancelled() {
        let mut input = session.hook_input(mode);
        input["tool_name"] = json!(call.name);
        input["tool_input"] = call.arguments.clone();
        input["tool_response"] = json!({"output": clip_middle(&text, MAX_CARD_CHARS), "is_error": out.is_error});
        let r = claude::run_hooks(&setup.hooks, "PostToolUse", Some(&call.name), input, &session.cwd).await;
        session.hook_notes(&r.notes);
        let p = session.prompts();
        if let Some(reason) = r.block.or(r.stop) {
            out.content.push(Part::Text(p.msg("hook_feedback", &[("reason", &reason)])));
        }
        for c in r.context {
            out.content.push(Part::Text(p.msg("hook_context", &[("text", &c)])));
        }
    }
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
    fn only_a_call_the_limit_cut_short_is_held_back() {
        let call = |args: Value| ToolCall { id: "c".into(), name: "Write".into(), arguments: args, signature: None };
        let whole = call(json!({"file_path": "a", "content": "b"}));
        let half = call(json!(r#"{"file_path": "a""#));
        let none = call(json!({}));
        assert_eq!(cut_off_call(&[whole.clone(), half.clone()], &StopReason::MaxTokens), Some(1));
        assert_eq!(cut_off_call(&[none], &StopReason::MaxTokens), Some(0));
        // Finished JSON is a finished call, even when the reply was cut off after it.
        assert_eq!(cut_off_call(&[half.clone(), whole.clone()], &StopReason::MaxTokens), None);
        assert_eq!(cut_off_call(&[half], &StopReason::ToolUse), None);
        assert_eq!(cut_off_call(&[], &StopReason::MaxTokens), None);
    }

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
