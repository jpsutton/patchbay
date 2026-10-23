//! Task, SendMessage and TaskStop: start subagents, talk to them, stop them.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use patchbay_llm::{Message, Part, Role, SystemBlock, ToolSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::session::{Mode, Session};
use crate::subagent::{self, Kind, Note, Status, SubAgent};
use crate::prompt;

const READ_ONLY: [&str; 5] = ["Read", "Glob", "Grep", "WebFetch", "WebSearch"];
/// What a subagent never gets: it cannot ask the user, plan, track tasks for the main agent or
/// start subagents of its own.
const WITHHELD: [&str; 7] = ["Task", "SendMessage", "TaskStop", "TodoWrite", "ExitPlanMode", "AskUserQuestion", "KillShell"];

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn kind_of(input: &Value) -> Kind {
    match str_arg(input, "subagent_type") {
        Some("explore") => Kind::Explore,
        Some("fork") => Kind::Fork,
        _ => Kind::General,
    }
}

/// How an agent run ended, for the main agent: the report, plus where its worktree is.
async fn conclude(parent: &Arc<Session>, agent: &Arc<SubAgent>, result: Result<String, String>) -> (Status, String) {
    let (status, mut text) = match result {
        Ok(t) => (Status::Done, t),
        Err(e) if e == "stopped" => (Status::Stopped, "The subagent was stopped before it finished.".to_owned()),
        Err(e) => (Status::Failed(e.clone()), e),
    };
    let worktree = agent.worktree.lock().unwrap().take();
    if let Some(w) = worktree {
        if let Ok(Some(note)) = tokio::task::spawn_blocking(move || subagent::finish_worktree(&w)).await {
            text.push_str(&format!("\n\n{note}"));
        }
    }
    *agent.status.lock().unwrap() = status.clone();
    let _ = parent;
    (status, text)
}

/// Run `agent` in the background; its report becomes a notification.
fn spawn_background(parent: Arc<Session>, agent: Arc<SubAgent>) {
    agent.background.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        let token = agent.cancel.lock().unwrap().clone();
        let result = subagent::run(&parent, &agent, &token).await;
        let (status, text) = conclude(&parent, &agent, result).await;
        parent.agents.push_note(Note {
            agent_id: agent.name.clone().unwrap_or_else(|| agent.id.clone()),
            description: agent.description.clone(),
            status: match status {
                Status::Done => "completed",
                Status::Stopped => "stopped",
                _ => "failed",
            },
            text,
        });
    });
}

/// Starts subagents. `custom` holds the user's own subagent types (`.claude/agents`), by name
/// and description.
#[derive(Default)]
pub struct Task {
    pub custom: Vec<(String, String)>,
}

#[async_trait]
impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        let mut types = vec!["explore".to_owned(), "general-purpose".to_owned(), "fork".to_owned()];
        types.extend(self.custom.iter().map(|(n, _)| n.clone()));
        let mut type_doc = "Defaults to general-purpose".to_owned();
        if !self.custom.is_empty() {
            type_doc.push_str(". The user's own subagents, each with its own instructions and tools:");
            for (n, d) in &self.custom {
                type_doc.push_str(&format!("\n- {n}: {d}"));
            }
        }
        ToolSpec {
            name: "Task".into(),
            description: "Start a subagent for a self-contained job. Types: `explore` (read-only: Read, Glob, \
Grep, WebFetch, WebSearch) to find things across many files or answer a question about the code; \
`general-purpose` for a multi-step job that needs the shell or edits; `fork` to branch off from this \
conversation with everything said so far as context, for one directive. Others start with no memory of the \
conversation, so give them the whole task: the goal, where to look, what to report. Start several \
`explore` subagents in one message to search in parallel. Set `run_in_background` to carry on while it \
works: its report arrives later as a task notification, so do not wait or poll. Give it a `name` to send it \
follow-ups with SendMessage. `isolation: \"worktree\"` runs it in its own git worktree so its edits do not touch \
this checkout. A subagent cannot ask the user questions or start subagents. Do not use it for a lookup you can \
do with one Grep or Read."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "A short label for the job (3-5 words)"},
                    "prompt": {"type": "string", "description": "The full task for the subagent"},
                    "subagent_type": {"type": "string", "enum": types, "description": type_doc},
                    "name": {"type": "string", "description": "Makes the subagent addressable by SendMessage (letters, digits, - and _)"},
                    "run_in_background": {"type": "boolean", "description": "Return at once; the report comes as a notification"},
                    "isolation": {"type": "string", "enum": ["worktree"], "description": "Run in a separate git worktree"}
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

    /// Searches and background jobs run side by side; ones that may edit files take turns.
    fn concurrent(&self, input: &Value) -> bool {
        kind_of(input) == Kind::Explore || bool_arg(input, "run_in_background")
    }

    fn title(&self, input: &Value) -> String {
        format!("Subagent: {}", str_arg(input, "description").unwrap_or("task"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(task) = str_arg(&input, "prompt").filter(|p| !p.trim().is_empty()) else {
            return ToolOutput::error("prompt is required");
        };
        let parent = &ctx.session;
        let description = str_arg(&input, "description").unwrap_or("subagent task").to_owned();
        let mode = parent.state.lock().await.mode;
        // In plan mode nothing may change, so only searching subagents start.
        let kind = if mode == Mode::Plan { Kind::Explore } else { kind_of(&input) };
        let setup = parent.claude();
        // One of the user's own subagent types: a general subagent with its instructions and tools.
        let custom = (kind == Kind::General)
            .then(|| str_arg(&input, "subagent_type"))
            .flatten()
            .and_then(|t| setup.agents.iter().find(|a| a.name == t).cloned());
        let name = match str_arg(&input, "name").filter(|n| !n.is_empty()) {
            Some(n) if !valid_name(n) => return ToolOutput::error("name may use letters, digits, - and _ (up to 40)"),
            Some(n) if parent.agents.name_taken(n) => return ToolOutput::error(format!("a subagent named {n} already exists; pick another name or SendMessage it")),
            Some(n) => Some(n.to_owned()),
            None => None,
        };
        let id = parent.agents.next_id();

        let (worktree, cwd) = if str_arg(&input, "isolation") == Some("worktree") && kind != Kind::Explore {
            let (data, cwd) = (parent.host.data_dir(), parent.shells.cwd(&parent.cwd));
            let id2 = id.clone();
            match tokio::task::spawn_blocking(move || subagent::create_worktree(&cwd, &data, &id2)).await {
                Ok(Ok((w, dir))) => (Some(w), dir),
                Ok(Err(e)) => return ToolOutput::error(e),
                Err(e) => return ToolOutput::error(e.to_string()),
            }
        } else {
            (None, parent.cwd.clone())
        };

        let settings = parent.host.settings();
        let p = parent.prompts();
        let mut registry = Registry::builtin(settings.web_fetch, parent.host.web_search_key().is_some());
        if kind != Kind::Explore {
            for t in parent.mcp.lock().await.tools() {
                registry.push(t);
            }
        }
        if !setup.skills.is_empty() {
            registry.push(Arc::new(super::skill::Skill));
        }
        let allowed = custom.as_ref().and_then(|c| c.tools.clone());
        registry.retain(|n| {
            if kind == Kind::Explore {
                READ_ONLY.contains(&n) || n == "Skill"
            } else {
                !WITHHELD.contains(&n) && allowed.as_ref().is_none_or(|a| a.iter().any(|x| x == n))
            }
        });
        let extras = prompt::Extras { claude: settings.claude.clone(), skills: &setup.skills };

        let model_id = parent.state.lock().await.model_id.clone();
        let model_name = parent.host.models().into_iter().find(|m| m.id == model_id).map(|m| m.name).unwrap_or(model_id);
        let (system, mut history) = match kind {
            // The main agent's own prompt and everything said before this step.
            Kind::Fork => {
                let mut h = parent.state.lock().await.messages.clone();
                // The last message is the one holding this very call, which has no result yet.
                h.pop();
                (prompt::system_blocks(&cwd, &model_name, &settings.instructions, mode, p, &extras), h)
            }
            _ => {
                let mut persona = custom.as_ref().map_or_else(|| p.subagent().trim().to_owned(), |c| c.prompt.clone());
                if let Some(terse) = p.terse() {
                    persona.push_str("\n\n");
                    persona.push_str(terse.trim());
                }
                let mut s = vec![SystemBlock::cached(persona)];
                s.extend(prompt::system_blocks(&cwd, &model_name, "", mode, p, &extras).pop());
                (s, Vec::new())
            }
        };
        let first = if kind == Kind::Fork {
            Message::with_parts(Role::User, vec![Part::Text(prompt::reminder(&p.msg("fork_note", &[]))), Part::Text(task.to_owned())])
        } else {
            Message::user(task)
        };
        history.push(first);
        let child_mode = if kind == Kind::Explore { Mode::Plan } else { Mode::Default };
        let child = parent.child(cwd, format!("{id}:"), history, child_mode).await;
        // A custom subagent's model, when the host has one by that id or name (`sonnet` matches
        // "Claude Sonnet 4.5"); otherwise it runs on the main agent's.
        if let Some(want) = custom.as_ref().and_then(|c| c.model.as_deref()).map(str::to_lowercase) {
            let models = parent.host.models();
            if let Some(m) = models.iter().find(|m| m.id.to_lowercase() == want).or_else(|| models.iter().find(|m| m.name.to_lowercase().contains(&want))) {
                child.state.lock().await.model_id = m.id.clone();
            }
        }
        let agent = Arc::new(SubAgent {
            id: id.clone(),
            name: name.clone(),
            kind,
            description,
            child,
            system,
            registry,
            status: std::sync::Mutex::new(Status::Running),
            inbox: Default::default(),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
            worktree: std::sync::Mutex::new(worktree),
            background: Default::default(),
        });
        parent.agents.add(agent.clone());
        let handle = name.as_deref().map(|n| format!("{id} \"{n}\"")).unwrap_or_else(|| id.clone());

        if bool_arg(&input, "run_in_background") {
            spawn_background(parent.clone(), agent);
            return ToolOutput::text(p.msg("bg_started", &[("handle", &handle), ("kind", kind.label())]));
        }
        let result = subagent::run(parent, &agent, &ctx.cancel).await;
        if ctx.cancel.is_cancelled() {
            let _ = conclude(parent, &agent, Err("stopped".into())).await;
            return ToolOutput::error("Cancelled.");
        }
        let failed = result.is_err();
        let (_, text) = conclude(parent, &agent, result).await;
        let out = format!("{text}\n\n{}", p.msg("continue_hint", &[("handle", &handle)]));
        if failed { ToolOutput::error(out) } else { ToolOutput::text(out) }
    }
}

pub struct SendMessage;

#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "SendMessage".into(),
            description: "Send a message to a subagent you started, by its id or name. A subagent still running in \
the background reads the message before its next step. One that has finished is continued with its \
whole history and the message as its next instruction, and its new report comes back. Set \
`run_in_background` to continue a finished one without waiting."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The subagent's id or name"},
                    "message": {"type": "string", "description": "What to tell it"},
                    "run_in_background": {"type": "boolean", "description": "Continue a finished subagent in the background"}
                },
                "required": ["to", "message"]
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
        format!("Message {}", str_arg(input, "to").unwrap_or("a subagent"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(to), Some(message)) = (str_arg(&input, "to"), str_arg(&input, "message").filter(|m| !m.trim().is_empty())) else {
            return ToolOutput::error("to and message are required");
        };
        let parent = &ctx.session;
        let Some(agent) = parent.agents.find(to) else {
            return ToolOutput::error(format!("No subagent {to}. Start one with Task."));
        };
        if *agent.status.lock().unwrap() == Status::Running {
            agent.inbox.lock().unwrap().push(message.to_owned());
            return ToolOutput::text(format!("Message queued for {to}; it reads it before its next step."));
        }
        // Continue a finished subagent from where it stopped.
        agent.child.state.lock().await.messages.push(Message::user(message));
        *agent.status.lock().unwrap() = Status::Running;
        *agent.cancel.lock().unwrap() = CancellationToken::new();
        if bool_arg(&input, "run_in_background") {
            let label = agent.kind.label();
            spawn_background(parent.clone(), agent);
            return ToolOutput::text(parent.prompts().msg("bg_continuing", &[("to", to), ("kind", label)]));
        }
        let result = subagent::run(parent, &agent, &ctx.cancel).await;
        if ctx.cancel.is_cancelled() {
            let _ = conclude(parent, &agent, Err("stopped".into())).await;
            return ToolOutput::error("Cancelled.");
        }
        let failed = result.is_err();
        let (_, text) = conclude(parent, &agent, result).await;
        if failed { ToolOutput::error(text) } else { ToolOutput::text(text) }
    }
}

pub struct TaskStop;

#[async_trait]
impl Tool for TaskStop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "TaskStop".into(),
            description: "Stop a subagent that is still running in the background, by its id or name.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "The subagent's id or name"}},
                "required": ["task_id"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "execute"
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Stop {}", str_arg(input, "task_id").unwrap_or("a subagent"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(id) = str_arg(&input, "task_id") else { return ToolOutput::error("task_id is required") };
        match ctx.session.agents.find(id) {
            Some(a) if *a.status.lock().unwrap() == Status::Running => {
                a.cancel.lock().unwrap().cancel();
                ToolOutput::text(format!("Stopping {id}."))
            }
            Some(_) => ToolOutput::text(format!("{id} is not running.")),
            None => ToolOutput::error(format!("No subagent {id}.")),
        }
    }
}
