//! The tools the agent offers the model.

use std::collections::VecDeque;
use std::ffi::OsString;
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
pub mod shell_lint;
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
    /// Offered to the model but refused when called (see [`Registry::refuse`]).
    refused: Vec<String>,
}

/// A tool still offered to the model but refused when called: a fork keeps the main agent's exact
/// tool list so its request shares the main agent's cached prefix.
struct Refused(Arc<dyn Tool>);

#[async_trait]
impl Tool for Refused {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }
    fn kind(&self) -> &'static str {
        self.0.kind()
    }
    fn title(&self, input: &Value) -> String {
        self.0.title(input)
    }
    fn allowed_in_plan(&self) -> bool {
        self.0.allowed_in_plan()
    }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::error(ctx.session.prompts().msg("fork_refused", &[("name", &self.0.spec().name)]))
    }
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
            Arc::new(task::TaskStatus),
        ];
        if web_fetch {
            tools.push(Arc::new(web::WebFetch));
        }
        if web_search {
            tools.push(Arc::new(web::WebSearch));
        }
        Registry { tools, refused: Vec::new() }
    }

    /// Keep offering the tools named in `names`, but refuse every call to them.
    pub fn refuse(&mut self, names: &[&str]) {
        self.refused.extend(names.iter().map(|n| (*n).to_owned()));
    }

    /// Whether the tool named `name` is on offer.
    pub fn offers(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.spec().name == name)
    }

    /// Keep only the tools whose name `keep` accepts.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.tools.retain(|t| keep(&t.spec().name));
    }

    pub fn push(&mut self, t: Arc<dyn Tool>) {
        self.tools.push(t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let t = self.tools.iter().find(|t| t.spec().name == name).cloned()?;
        if self.refused.iter().any(|r| r == name) {
            return Some(Arc::new(Refused(t)));
        }
        Some(t)
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

/// Whether `p` lies inside `root`.
///
/// Both are resolved the way the filesystem resolves them ([`resolve`]), so a symlink
/// followed by `..` leaves for where the link points instead of staying in the directory
/// that holds the link — the escape `Session::may_read` gates on.
pub fn inside(root: &Path, p: &Path) -> bool {
    resolve(p).starts_with(&resolve(root))
}

/// One step of [`resolve`], owned so a symlink's target can be spliced in front of the
/// steps still to come.
enum Step {
    Prefix(OsString),
    Root(OsString),
    /// `.`: the directory already resolved.
    Here,
    /// `..`: the directory above.
    Parent,
    Name(OsString),
}

impl Step {
    fn of(c: Component<'_>) -> Step {
        match &c {
            Component::Prefix(p) => Step::Prefix(p.as_os_str().to_os_string()),
            Component::RootDir => Step::Root(c.as_os_str().to_os_string()),
            Component::CurDir => Step::Here,
            Component::ParentDir => Step::Parent,
            Component::Normal(n) => Step::Name(n.to_os_string()),
        }
    }
}

/// `p` as the filesystem would see it: every step applied to the path resolved so far,
/// and every symlink followed before the next step — `..` in particular — is applied, so
/// `link/..` moves to where the link points rather than to the directory holding it.
/// Steps that do not exist yet stay as they are, so a file about to be written is judged
/// by the path it will have; a path that exists ends up canonicalized, which settles
/// case on a case-insensitive filesystem.
fn resolve(p: &Path) -> PathBuf {
    // A relative path means "from here", and `..` may climb out of here.
    let mut real = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().map(|d| d.canonicalize().unwrap_or(d)).unwrap_or_default()
    };
    // Splice a link's target in front of the steps still to come, so the rest of the path
    // lands where the link points. The budget bounds links that point at each other; past
    // it the link stays by name, which can only ever make `inside` answer no.
    let mut hops = 64;
    let mut queue: VecDeque<Step> = p.components().map(Step::of).collect();
    while let Some(step) = queue.pop_front() {
        match step {
            Step::Prefix(d) => real = PathBuf::from(d),
            Step::Root(r) => {
                // On Windows the root completes the drive named just above it; anywhere
                // else it is the path's own root and replaces what came before it.
                if !real.has_root() && matches!(real.components().next(), Some(Component::Prefix(_))) {
                    real.push(r);
                } else {
                    real = PathBuf::from(r);
                }
            }
            Step::Here => {}
            Step::Parent => {
                real.pop();
            }
            Step::Name(n) => {
                real.push(n);
                if hops > 0
                    && std::fs::symlink_metadata(&real).is_ok_and(|m| m.file_type().is_symlink())
                    && let Ok(target) = std::fs::read_link(&real)
                {
                    hops -= 1;
                    real.pop();
                    let mut target: VecDeque<Step> = target.components().map(Step::of).collect();
                    target.append(&mut queue);
                    queue = target;
                }
            }
        }
    }
    real.canonicalize().unwrap_or(real)
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
    fn refused_tools_stay_offered() {
        let all = Registry::builtin(false, false);
        let mut fork = Registry::builtin(false, false);
        fork.refuse(&["Task", "AskUserQuestion"]);
        let names = |r: &Registry| r.specs(false, crate::prompts::Prompts::new(false)).into_iter().map(|s| s.name).collect::<Vec<_>>();
        // Same list, same order: the fork's request keeps the main agent's cached prefix.
        assert_eq!(names(&all), names(&fork));
        // A refused tool comes back wrapped; the rest are the registry's own.
        let task = fork.get("Task").unwrap();
        assert!(!Arc::ptr_eq(&task, &fork.get("Task").unwrap()));
        assert_eq!(task.spec().name, "Task");
        assert!(Arc::ptr_eq(&fork.get("Read").unwrap(), &fork.get("Read").unwrap()));
    }

    #[test]
    fn insideness_folds_dotdot() {
        let root = std::env::temp_dir();
        assert!(inside(&root, &root.join("a/b.txt")));
        assert!(inside(&root, &root.join("a/../b.txt")));
        assert!(!inside(&root, &root.join("../outside.txt")));
        assert!(!inside(&root, Path::new("/etc/passwd")));
    }

    /// A symlink inside the root plus `..` must not walk out of it. The filesystem
    /// resolves `root/link/..` as "wherever the link points, then up one", so judging
    /// `link/..` as if it sat in `root` hands out paths that live outside the root —
    /// this is the gap `Session::may_read` inherits, since it gates on `inside`.
    #[test]
    #[cfg(unix)]
    fn a_symlink_cannot_smuggle_dotdot_out_of_root() {
        let base = std::env::temp_dir().join(format!("pb-inside-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("stolen.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside"), root.join("link")).unwrap();

        // `root/link/../stolen.txt` is `outside/../stolen.txt` = `base/stolen.txt`: outside
        // the root, and the file is really there, so a read through it would succeed.
        assert!(!inside(&root, &root.join("link/../stolen.txt")), "symlink + .. escaped the root");
        // The link alone names a path outside the root too.
        assert!(!inside(&root, &root.join("link/anything.txt")));

        // A dangling link is still a link: its target is where an access would land.
        std::os::unix::fs::symlink(base.join("nowhere"), root.join("dangling")).unwrap();
        assert!(!inside(&root, &root.join("dangling")), "a dangling link points outside");
        assert!(!inside(&root, &root.join("dangling/../stolen.txt")));

        // What is genuinely inside still passes, `..` and all.
        assert!(inside(&root, &root.join("nested/file.txt")));
        assert!(inside(&root, &root.join("nested/../nested/file.txt")));
        assert!(inside(&root, &root));
        // A link that points back inside stays inside.
        std::os::unix::fs::symlink(root.join("nested"), root.join("inlink")).unwrap();
        assert!(inside(&root, &root.join("inlink/file.txt")));
        assert!(inside(&root, &root.join("inlink/../nested/file.txt")));
    }

    #[test]
    fn clips_the_middle() {
        let s = "a".repeat(100);
        let c = clip_middle(&s, 20);
        assert!(c.starts_with("aaaaaaaaaa\n…[80 characters cut]…\naaaaaaaaaa"), "{c}");
        assert_eq!(clip_middle("short", 20), "short");
    }

    // --- pb-20261111-71: the sandbox boundary on Windows -----------------------------------
    // Until now no test exercised an ESCAPE on Windows: the symlink test is `cfg(unix)`, and on
    // the Windows runner the two tests that fail (`insideness_folds_dotdot`, the `may_read` one)
    // stop at their first, in-root assert, so the escape asserts after it never ran. These run
    // the escape directions on their own, on every platform, and add the Windows-only ones
    // (a junction, a directory symlink, an 8.3 short-name alias, mixed separators).

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("pb-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// `..` out of the root is outside, however it is written.
    #[test]
    fn dotdot_out_of_the_root_is_not_inside() {
        let base = scratch("dotdot");
        let root = base.join("work");
        std::fs::create_dir_all(root.join("a")).unwrap();
        assert!(!inside(&root, &root.join("../outside.txt")));
        assert!(!inside(&root, &root.join("a/../../outside.txt")));
        assert!(!inside(&root, &root.join("a/../..")));
        // A sibling whose name starts with the root's name is not under it.
        assert!(!inside(&root, &base.join("work-evil/x.txt")));
        // The control: staying under the root with `..` that does not climb out. The path
        // exists, so both sides resolve alike (a path that does not exist yet is the over-deny).
        std::fs::write(root.join("a/in.txt"), "x").unwrap();
        assert!(inside(&root, &root.join("a/../a/in.txt")));
    }

    /// An absolute path somewhere else is outside.
    #[test]
    fn an_absolute_path_elsewhere_is_not_inside() {
        let base = scratch("elsewhere");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        let elsewhere = PathBuf::from("/etc/passwd");
        #[cfg(windows)]
        let elsewhere = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())).join("System32");
        assert!(!inside(&root, &elsewhere));
    }

    /// Windows accepts `\` and `/` in one path: `..` written either way must climb out all the same.
    #[test]
    #[cfg(windows)]
    fn dotdot_with_mixed_separators_is_not_inside() {
        let base = scratch("mixed");
        let root = base.join("work");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(base.join("stolen.txt"), "s").unwrap();
        let r = root.display();
        for escape in [
            format!("{r}\\a/../..\\stolen.txt"),
            format!("{r}/a\\..\\../stolen.txt"),
            format!("{r}\\..\\stolen.txt"),
            format!("{r}/../stolen.txt"),
            format!("{r}\\a\\..\\..\\stolen.txt"),
        ] {
            assert!(!inside(&root, Path::new(&escape)), "mixed separators escaped the root: {escape}");
        }
        // The control: the same mixture that stays under the root, on a file that exists.
        std::fs::write(root.join("a/in.txt"), "x").unwrap();
        assert!(inside(&root, Path::new(&format!("{r}\\a/../a\\in.txt"))));
    }

    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        // `mklink /J` makes a directory junction and needs no privilege.
        let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
        assert!(out.status.success(), "mklink /J failed: {} {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }

    /// A junction inside the root that points outside it: what lies behind it is outside, whether
    /// the file exists or is about to be written.
    #[test]
    #[cfg(windows)]
    fn a_junction_out_of_the_root_is_not_inside() {
        let base = scratch("junction");
        let root = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::fs::write(base.join("stolen.txt"), "s").unwrap();
        std::fs::write(root.join("real/in.txt"), "x").unwrap();
        junction(&root.join("link"), &outside);

        assert!(!inside(&root, &root.join("link/secret.txt")), "an existing file behind a junction");
        assert!(!inside(&root, &root.join("link/newfile.txt")), "a new file behind a junction");
        assert!(!inside(&root, &root.join("link")), "the junction itself names a place outside");
        assert!(!inside(&root, &root.join("link/../stolen.txt")), "junction + .. escaped the root");
        // The controls: a real directory under the root stays inside, so the asserts above are not vacuous.
        assert!(inside(&root, &root.join("real/in.txt")));
        // A junction that points back inside stays inside.
        junction(&root.join("inlink"), &root.join("real"));
        assert!(inside(&root, &root.join("inlink/in.txt")));
    }

    /// A directory symlink needs a privilege (or developer mode) to create: where it cannot be made
    /// the test says so and returns; the GitHub runner can.
    #[test]
    #[cfg(windows)]
    fn a_directory_symlink_out_of_the_root_is_not_inside() {
        let base = scratch("dirsym");
        let root = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        if let Err(e) = std::os::windows::fs::symlink_dir(&outside, root.join("link")) {
            eprintln!("skipped: cannot create a directory symlink here ({e})");
            return;
        }
        assert!(!inside(&root, &root.join("link/secret.txt")));
        assert!(!inside(&root, &root.join("link/newfile.txt")));
        assert!(!inside(&root, &root.join("link")));
    }

    #[cfg(windows)]
    fn short_path(p: &Path) -> PathBuf {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
        }
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` is as long as the length passed.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        assert!(n > 0 && n < buf.len(), "GetShortPathNameW failed for {}", p.display());
        PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
    }

    /// A place outside the root reached by its 8.3 short name is still outside, and a place inside
    /// the root reached by its short name is still inside (both sides must agree on the spelling).
    #[test]
    #[cfg(windows)]
    fn an_8_3_short_name_alias_cannot_hide_a_place_outside_the_root() {
        let base = scratch("short");
        let root = base.join("work_directory_with_a_long_name");
        let outside = base.join("outside_directory_with_a_long_name");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::fs::write(root.join("in.txt"), "x").unwrap();
        let (short_outside, short_root) = (short_path(&outside), short_path(&root));
        if short_outside == outside || short_root == root {
            eprintln!("skipped: this volume does not make 8.3 short names");
            return;
        }
        // Outside, by its short name: an existing file and one about to be written.
        assert!(!inside(&root, &short_outside.join("secret.txt")), "existing file via an 8.3 alias");
        assert!(!inside(&root, &short_outside.join("newfile.txt")), "new file via an 8.3 alias");
        assert!(!inside(&short_root, &outside.join("secret.txt")), "root by short name, file by long name");
        // Inside, by either spelling of either side, for a file that exists.
        assert!(inside(&root, &short_root.join("in.txt")));
        assert!(inside(&short_root, &root.join("in.txt")));
        assert!(inside(&short_root, &short_root.join("in.txt")));
    }

}
