//! TodoWrite and ExitPlanMode.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::Value;

use super::*;
use crate::session::{Mode, Session, Todo};

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "TodoWrite".into(),
            description: "Keep a task list for the current session. Use it for work with three or more steps \
or several parts. Send the whole list every time. Exactly one task is `in_progress`; mark a task \
`completed` as soon as it is done, never while tests fail or the work is partial. Each task has an \
imperative `content` (\"Run the tests\") and a present-continuous `activeForm` (\"Running the tests\")."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                                "activeForm": {"type": "string"}
                            },
                            "required": ["content", "status", "activeForm"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "think"
    }

    fn read_only(&self) -> bool {
        false
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, _: &Value) -> String {
        "Update the task list".into()
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let todos: Vec<Todo> = match serde_json::from_value(input.get("todos").cloned().unwrap_or(Value::Null)) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(format!("todos must be a list of {{content, status, activeForm}}: {e}")),
        };
        if todos.iter().any(|t| !matches!(t.status.as_str(), "pending" | "in_progress" | "completed")) {
            return ToolOutput::error("status must be pending, in_progress or completed");
        }
        let entries: Vec<Value> = todos
            .iter()
            .map(|t| {
                json!({
                    "content": if t.status == "in_progress" && !t.active_form.is_empty() { &t.active_form } else { &t.content },
                    "priority": "medium",
                    "status": t.status,
                })
            })
            .collect();
        ctx.session.update(json!({"sessionUpdate": "plan", "entries": entries}));
        ctx.session.state.lock().await.todos = todos;
        ToolOutput::text(
            "Todos have been modified successfully. Continue to use the list to track progress and carry on with the current task.",
        )
    }
}

pub struct ExitPlanMode;

#[async_trait]
impl Tool for ExitPlanMode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ExitPlanMode".into(),
            description: "In plan mode, present your plan and ask the user to approve it. Call it once the \
plan is ready, and only for tasks that involve writing code, not for research. The user answers in the \
app; do not ask for approval any other way. After approval you leave plan mode and may change files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"plan": {"type": "string", "description": "The plan, in Markdown"}},
                "required": ["plan"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "switch_mode"
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, _: &Value) -> String {
        "Ready to code?".into()
    }

    /// Always asked, with its own options (see `turn::ask`).
    fn needs_permission(&self, _: &Value, _: &Session) -> bool {
        true
    }

    async fn preview(&self, input: &Value, _: &Session) -> Vec<Value> {
        let plan = str_arg(input, "plan").unwrap_or("");
        vec![json!({"type": "content", "content": {"type": "text", "text": plan}})]
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let mut st = ctx.session.state.lock().await;
        if st.mode != Mode::Plan {
            return ToolOutput::error("The session is not in plan mode.");
        }
        st.mode = Mode::Default;
        let model = st.model_id.clone();
        drop(st);
        ctx.session.save_meta(&model, Mode::Default);
        ctx.session.update(json!({"sessionUpdate": "current_mode_update", "currentModeId": Mode::Default.id()}));
        let plan = str_arg(&input, "plan").unwrap_or("");
        ToolOutput::text(format!(
            "The user approved the plan. You may now change files and run commands. Start by updating the task list.\n\nApproved plan:\n{plan}"
        ))
    }
}
