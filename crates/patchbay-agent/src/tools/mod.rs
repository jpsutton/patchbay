//! The tools the agent offers the model.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use patchbay_llm::{Part, ToolSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::session::Session;

pub mod ask;
pub mod fs;
pub mod misc;
pub mod notebook;
pub mod search;
pub mod shell;
pub mod skill;
pub mod task;
pub mod web;

/// What a tool hands back: the model's view, the client's view, and raw data for the card.
pub struct ToolOutput {
    /// What the model reads: `Text` and `Image` parts.
    pub content: Vec<Part>,
    /// ACP tool-call content for the card; empty means "show the text".
    pub ui: Vec<Value>,
    pub raw: Value,
    pub is_error: bool,
    /// Merged into the final update's `_meta` (a command's exit code).
    pub meta: Option<Value>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput { content: vec![Part::Text(s.into())], ui: Vec::new(), raw: Value::Null, is_error: false, meta: None }
    }

    pub fn error(s: impl Into<String>) -> Self {
        ToolOutput { is_error: true, ..Self::text(s) }
    }

    pub fn with_ui(mut self, ui: Vec<Value>) -> Self {
        self.ui = ui;
        self
    }

    pub fn text_of(&self) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let Part::Text(t) = p {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        out
    }
}

pub struct ToolCtx {
    pub session: Arc<Session>,
    pub call_id: String,
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// ACP tool kind: read, edit, delete, move, search, execute, think, fetch, switch_mode, other.
    fn kind(&self) -> &'static str;
    fn title(&self, input: &Value) -> String;
    fn locations(&self, _input: &Value, _session: &Session) -> Vec<String> {
        Vec::new()
    }
    /// Safe to run next to other read-only calls.
    fn read_only(&self) -> bool {
        false
    }
    /// Whether this call may run beside other calls of the same step.
    fn concurrent(&self, _input: &Value) -> bool {
        self.read_only()
    }
    /// Plan mode offers only tools that report true.
    fn allowed_in_plan(&self) -> bool {
        self.read_only()
    }
    /// Whether the client must approve this call first.
    fn needs_permission(&self, _input: &Value, _session: &Session) -> bool {
        false
    }
    /// ACP content for the permission request (a diff of the change).
    async fn preview(&self, _input: &Value, _session: &Session) -> Vec<Value> {
        Vec::new()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput;
}

pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn builtin(web_fetch: bool, web_search: bool) -> Registry {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(fs::Read),
            Arc::new(fs::Write),
            Arc::new(fs::Edit),
            Arc::new(search::Glob),
            Arc::new(search::Grep),
            Arc::new(shell::Bash),
            Arc::new(shell::BashOutput),
            Arc::new(shell::KillShell),
            Arc::new(notebook::NotebookEdit),
            Arc::new(misc::TodoWrite),
            Arc::new(misc::ExitPlanMode),
            Arc::new(ask::AskUserQuestion),
            Arc::new(task::Task::default()),
            Arc::new(task::SendMessage),
            Arc::new(task::TaskStop),
        ];
        if web_fetch {
            tools.push(Arc::new(web::WebFetch));
        }
        if web_search {
            tools.push(Arc::new(web::WebSearch));
        }
        Registry { tools }
    }

    /// Keep only the tools whose name `keep` accepts.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.tools.retain(|t| keep(&t.spec().name));
    }

    pub fn push(&mut self, t: Arc<dyn Tool>) {
        self.tools.push(t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }

    /// The tools to offer, with minified descriptions when `p` asks for them.
    pub fn specs(&self, plan_mode: bool, p: crate::prompts::Prompts) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|t| !plan_mode || t.allowed_in_plan())
            .map(|t| {
                let mut s = t.spec();
                s.description = p.tool_description(&s.name, std::mem::take(&mut s.description));
                s
            })
            .collect()
    }
}

// --- input helpers ---

pub fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub fn u64_arg(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}

pub fn bool_arg(input: &Value, key: &str) -> bool {
    input.get(key).is_some_and(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
}

// --- paths ---

/// `p` with `.` and `..` folded away, no filesystem access.
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Whether `p` lies inside `root`, following symlinks as far as the path exists.
pub fn inside(root: &Path, p: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| lexical(root));
    let p = lexical(p);
    let mut existing = p.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => tail.push(n.to_owned()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut real = existing.canonicalize().unwrap_or(existing);
    real.extend(tail.iter().rev());
    real.starts_with(&root)
}

/// `p` relative to `root` when inside it, for short display.
pub fn display_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

/// Keep the head and tail of long output, with a note of how much was cut.
pub fn clip_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let half = max / 2;
    let mut head = half;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - half;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n…[{} characters cut]…\n{}", &s[..head], s.len() - head - (s.len() - tail), &s[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insideness_folds_dotdot() {
        let root = std::env::temp_dir();
        assert!(inside(&root, &root.join("a/b.txt")));
        assert!(inside(&root, &root.join("a/../b.txt")));
        assert!(!inside(&root, &root.join("../outside.txt")));
        assert!(!inside(&root, Path::new("/etc/passwd")));
    }

    #[test]
    fn clips_the_middle() {
        let s = "a".repeat(100);
        let c = clip_middle(&s, 20);
        assert!(c.starts_with("aaaaaaaaaa\n…[80 characters cut]…\naaaaaaaaaa"), "{c}");
        assert_eq!(clip_middle("short", 20), "short");
    }
}
