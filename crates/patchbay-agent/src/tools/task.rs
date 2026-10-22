//! Task: hand a self-contained job to a subagent and get its report back.

use async_trait::async_trait;
use patchbay_llm::{Message, Part, Request, Role, SystemBlock, ToolSpec};
use serde_json::{Value, json};

use super::*;
use crate::{prompt, turn};

const SUBAGENT: &str = include_str!("../../prompts/subagent.md");
/// Most model requests one subagent may make.
const MAX_STEPS: usize = 40;
const READ_ONLY: [&str; 5] = ["Read", "Glob", "Grep", "WebFetch", "WebSearch"];
/// What a subagent never gets: it cannot ask the user, plan, or start subagents of its own.
const WITHHELD: [&str; 4] = ["Task", "TodoWrite", "ExitPlanMode", "AskUserQuestion"];

pub struct Task;

#[async_trait]
impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Task".into(),
            description: "Start a subagent for a self-contained job and wait for its report. Use `explore` \
(read-only: Read, Glob, Grep, WebFetch) to find things across many files or answer a question about the \
code; use `general-purpose` for a multi-step job that needs the shell or edits. A subagent starts with \
no memory of this conversation, so give it the whole task: the goal, where to look, what to report. It \
cannot ask you questions. Only its final message comes back. Do not use it for a lookup you can do \
with one Grep or Read."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "A short label for the job (3-5 words)"},
                    "prompt": {"type": "string", "description": "The full task for the subagent"},
                    "subagent_type": {"type": "string", "enum": ["explore", "general-purpose"], "description": "Defaults to general-purpose"}
                },
                "required": ["description", "prompt"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "think"
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Subagent: {}", str_arg(input, "description").unwrap_or("task"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(task) = str_arg(&input, "prompt").filter(|p| !p.trim().is_empty()) else {
            return ToolOutput::error("prompt is required");
        };
        let explore = str_arg(&input, "subagent_type") == Some("explore");
        let session = &ctx.session;
        let (model_id, mode) = {
            let st = session.state.lock().await;
            (st.model_id.clone(), st.mode)
        };
        let settings = session.host.settings();
        let mc = match session.host.model_client(&model_id).await {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let mut registry = Registry::builtin(settings.web_fetch, session.host.web_search_key().is_some());
        registry.retain(|name| {
            if explore || mode == crate::session::Mode::Plan { READ_ONLY.contains(&name) } else { !WITHHELD.contains(&name) }
        });
        let mut system = mc.system_prefix.clone();
        system.push(SystemBlock::cached(SUBAGENT.trim()));
        let name = session.host.models().into_iter().find(|m| m.id == model_id).map(|m| m.name).unwrap_or(model_id);
        let env = prompt::system_blocks(&session.cwd, &name, "", mode).pop();
        system.extend(env);
        let mut messages = vec![Message::user(task)];
        for _ in 0..MAX_STEPS {
            let request = Request { system: system.clone(), messages: messages.clone(), tools: registry.specs(false), ..Default::default() };
            let response = match mc.client.complete(&request, &ctx.cancel, |_| {}).await {
                Ok(r) => r,
                Err(e) if e.cancelled => return ToolOutput::error("Cancelled."),
                Err(e) => return ToolOutput::error(format!("The subagent failed: {e}")),
            };
            if let Some(u) = response.usage {
                let mut t = session.extra_usage.lock().unwrap();
                t.input += u.input;
                t.cached_read += u.cached_read;
                t.cache_write += u.cache_write;
                t.output += u.output;
                t.reasoning += u.reasoning;
            }
            let calls: Vec<_> = response.message.tool_calls().cloned().collect();
            let text = response.message.text();
            messages.push(response.message);
            if calls.is_empty() {
                return if text.trim().is_empty() {
                    ToolOutput::error("The subagent finished without a report.")
                } else {
                    ToolOutput::text(text)
                };
            }
            let results = turn::execute(session, &registry, calls, &ctx.cancel, "↳ ").await;
            messages.push(Message::with_parts(Role::User, results.into_iter().map(Part::ToolResult).collect()));
        }
        ToolOutput::error("The subagent used up its step limit before it finished.")
    }
}
