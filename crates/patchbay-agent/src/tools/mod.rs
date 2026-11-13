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
///
/// What a path's PREFIX names (a drive, a share, a device) is compared first and on its own
/// ([`prefix_key`]): a path on another drive or another machine's share is outside whatever its
/// tail says, the way `\\?\C:\x` and `\\server\share\x` once read as the current drive's `\x`. And
/// judging `p` never touches another machine or a device namespace: [`resolve`] gives up (so the
/// answer is no) the moment the walk, including the target of a link, names a share that is not the
/// root's own or a device, before any filesystem call on it, so the check itself cannot make
/// Windows connect to an attacker's share and send it the user's credentials.
pub fn inside(root: &Path, p: &Path) -> bool {
    // The root is the session's own directory: trusted, it may be anywhere.
    let Some(root_r) = resolve(root, None) else { return false };
    let own = prefix_key(&root_r);
    let Some(p_r) = resolve(p, Some(&own)) else { return false };
    prefix_key(&p_r) == own && without_prefix(&p_r).starts_with(without_prefix(&root_r))
}

/// Whether `p` names another machine's share, a device or a volume namespace: a path that opens
/// with a pair of separators (a UNC share, `\\?\UNC\...`, `\\.\...`, `\\?\Volume{...}`, even `\\host`
/// alone) or with the NT prefix `\??\`, unless it is a verbatim drive (`\\?\C:\...`) or the share the
/// root `cwd` itself lives on. Windows takes either slash and any mix of them as a separator, so the
/// text is normalized first: `//host/share`, `\/host/share` and `/\host/share` are UNC too. `localhost`,
/// `127.0.0.1` and the machine's own name are foreign like any other host (a loopback connection can be
/// relayed). LEXICAL: it reads the text and calls nothing on the filesystem, so asking never connects
/// to anything. Off Windows a leading `//` or a backslash means nothing special: always false there.
pub fn foreign_location(cwd: &Path, p: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let n = p.to_string_lossy().replace('/', "\\");
    if n.starts_with(r"\??\") {
        return true; // the NT object namespace, which CreateFile passes straight through
    }
    if !n.starts_with(r"\\") {
        return false;
    }
    let key = prefix_key(Path::new(&n));
    if key.starts_with("disk:") {
        return false; // `\\?\C:\...`: a verbatim drive is local
    }
    let cwd_n = cwd.to_string_lossy().replace('/', "\\");
    !(key.starts_with("unc:") && key == prefix_key(Path::new(&cwd_n)))
}

/// The refusal for a raw path argument that names a network location or a device path
/// ([`foreign_location`]), judged on the TEXT before the path is resolved, joined, stat'ed or opened.
/// Every path-taking tool calls it first in `run`, so it holds however the tool was reached; the
/// dispatcher refuses the same paths before any hook or approval (`turn.rs`).
pub fn foreign_refusal(session: &Session, raw: &str) -> Option<String> {
    foreign_target(&session.cwd, Path::new(raw)).then(|| session.prompts().msg("foreign_location", &[("path", raw)]))
}

/// Most links followed in [`foreign_target`]; a chain longer than that is refused, not followed.
const LINK_HOPS: usize = 16;

/// [`foreign_location`] for the path AND for where any link on the way to it points: a link in the
/// project whose target is another machine's share (or a device path) is refused exactly like the
/// share written out, because the first thing Windows does with such a path (a canonicalize, a stat,
/// the search that walks it) is connect to the share and send the user's credentials.
///
/// LOCAL and lexical: each component is read with `symlink_metadata` (the link itself, never what it
/// points at) and `read_link`, and the TEXT of a target is judged; nothing is opened through a link,
/// and the walk stops at the first link that leads off the machine, so no component behind it is
/// touched. Links to links are followed up to [`LINK_HOPS`]. This is the one check every caller uses:
/// the dispatcher (`turn.rs`), each tool's own `foreign_refusal`, and the permission classifier
/// (`src-tauri/src/code/permissions.rs`).
pub fn foreign_target(cwd: &Path, p: &Path) -> bool {
    foreign_location(cwd, p) || (cfg!(windows) && link_leads_off_machine(cwd, p, LINK_HOPS))
}

fn link_leads_off_machine(cwd: &Path, p: &Path, hops: usize) -> bool {
    let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let mut real = PathBuf::new();
    for c in p.components() {
        match c {
            // Windows folds `..` in the text before it opens anything.
            Component::ParentDir => {
                real.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                real.push(name);
                if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.file_type().is_symlink()) {
                    continue;
                }
                let Ok(target) = std::fs::read_link(&real) else { continue };
                let target = if target.is_absolute() { target } else { real.parent().map(|d| d.join(&target)).unwrap_or(target) };
                if foreign_location(cwd, &target) || hops == 0 || link_leads_off_machine(cwd, &target, hops - 1) {
                    return true;
                }
            }
            other => real.push(other.as_os_str()),
        }
    }
    false
}

/// What a path's prefix names, for comparing two of them: `C:` and `\\?\C:` are one drive,
/// `\\server\share` and `\\?\UNC\server\share` one share, case does not matter. Anything else (a
/// device `\\.\X`, `\\?\GLOBALROOT`, a `\\?\Volume{GUID}`) is a namespace of its own. A path with no
/// prefix (everywhere but Windows) names "".
fn prefix_key(p: &Path) -> String {
    use std::path::Prefix::*;
    match p.components().next() {
        Some(Component::Prefix(pc)) => match pc.kind() {
            Disk(d) | VerbatimDisk(d) => format!("disk:{}", (d as char).to_ascii_uppercase()),
            UNC(server, share) | VerbatimUNC(server, share) => {
                format!("unc:{}\\{}", server.to_string_lossy().to_lowercase(), share.to_string_lossy().to_lowercase())
            }
            Verbatim(name) | DeviceNS(name) => format!("other:{}", name.to_string_lossy().to_lowercase()),
        },
        _ => String::new(),
    }
}

/// Whether a walk may go on through `prefix` (the first component of a path, or of a link's
/// target): a local drive, or the share the root itself is on (`own`). Another machine's share and
/// every device or volume namespace stop it before anything is opened.
fn prefix_allowed(prefix: &std::ffi::OsStr, own: &str) -> bool {
    let key = prefix_key(Path::new(prefix));
    key.starts_with("disk:") || (key.starts_with("unc:") && key == own)
}

/// `p` without its prefix: what is left to compare once the prefixes are known to be the same.
fn without_prefix(p: &Path) -> PathBuf {
    p.components().filter(|c| !matches!(c, Component::Prefix(_))).collect()
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
/// by the path it will have; the part that exists ends up canonicalized ([`canonical_prefix`]),
/// which settles case on a case-insensitive filesystem and, on Windows, the `\\?\` form and
/// the 8.3 short names, so a path and the root it sits in are spelled alike.
///
/// `own` is the prefix key of the root ([`prefix_key`]) for a path that is not trusted, `None` for
/// the root itself. `None` comes back when the walk meets a prefix it may not go through
/// ([`prefix_allowed`]) or a chain of links it cannot follow to the end: the caller reads either as
/// "not inside" without having opened anything it should not.
fn resolve(p: &Path, own: Option<&str>) -> Option<PathBuf> {
    // A relative path means "from here", and `..` may climb out of here.
    let mut real = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().map(|d| d.canonicalize().unwrap_or(d)).unwrap_or_default()
    };
    // Splice a link's target in front of the steps still to come, so the rest of the path
    // lands where the link points. The budget bounds links that point at each other; past
    // it the path is not judged at all (`None`), because leaving the link by name would let the
    // canonicalization at the end follow it, wherever it goes.
    let mut hops = 64;
    let mut queue: VecDeque<Step> = p.components().map(Step::of).collect();
    while let Some(step) = queue.pop_front() {
        match step {
            Step::Prefix(d) => {
                if own.is_some_and(|own| !prefix_allowed(&d, own)) {
                    return None;
                }
                real = PathBuf::from(d)
            }
            Step::Root(r) => {
                // On Windows the root completes the prefix named just above it: a drive (`C:`),
                // a verbatim drive (`\\?\C:`), a UNC share (`\\server\share`). The prefix stays:
                // dropping it left `\\?\C:\x` and `\\server\share\x` as a bare `\x`, which the
                // end of this function resolved against the CURRENT drive. Anywhere else the root
                // is the path's own and replaces what came before it.
                if matches!(real.components().next(), Some(Component::Prefix(_))) {
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
                if std::fs::symlink_metadata(&real).is_ok_and(|m| m.file_type().is_symlink())
                    && let Ok(target) = std::fs::read_link(&real)
                {
                    if hops == 0 {
                        return None;
                    }
                    hops -= 1;
                    real.pop();
                    let mut target: VecDeque<Step> = target.components().map(Step::of).collect();
                    target.append(&mut queue);
                    queue = target;
                }
            }
        }
    }
    Some(canonical_prefix(real))
}

/// `real` with its longest EXISTING ancestor canonicalized and the rest (not there yet, so no
/// link in it to follow) appended as written. A path that exists is canonicalized whole, as
/// before. The old code gave up on a path that did not exist and returned it uncanonicalized:
/// on Windows that left a short-name, non-verbatim spelling to be compared with the root's
/// canonical verbatim long-name one, so a file about to be written inside the root read as
/// outside it.
fn canonical_prefix(real: PathBuf) -> PathBuf {
    let mut tail = Vec::new();
    let mut head = real.as_path();
    loop {
        if let Ok(mut done) = head.canonicalize() {
            done.extend(tail.iter().rev());
            return done;
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                head = parent;
            }
            // Nothing of it exists (or it ends in `..`, which `resolve` has already applied).
            _ => return real,
        }
    }
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

    /// A skip is not a pass. On a developer's machine it says so and returns; in CI
    /// (`GITHUB_ACTIONS`) it fails, so a step the runner could not run shows up as unverified
    /// instead of as an `ok`, which a passing test's captured output would hide.
    #[cfg(windows)]
    fn skip(reason: &str) {
        assert!(std::env::var_os("GITHUB_ACTIONS").is_none(), "SKIPPED, this step is unverified here: {reason}");
        eprintln!("skipped: {reason}");
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
            skip(&format!("cannot create a directory symlink here ({e})"));
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
            skip("this volume does not make 8.3 short names");
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


    // --- pb-20261111-71: a path that does not exist yet --------------------------------------

    /// A file about to be written inside the root is inside it. Before, a path that did not exist
    /// was compared as written with a root that was canonicalized: on Windows the two spellings
    /// (`\\?\` and short names) never matched.
    #[test]
    fn a_new_file_in_the_root_is_inside() {
        let base = scratch("newfile");
        let root = base.join("work");
        std::fs::create_dir_all(root.join("a")).unwrap();
        assert!(inside(&root, &root.join("new.txt")));
        assert!(inside(&root, &root.join("a/new.txt")));
        assert!(inside(&root, &root.join("a/b/c/new.txt")), "several missing levels");
        assert!(inside(&root, &root.join("a/../a/new.txt")));
        // The same file with the escape asserts beside it, so "inside" is not just "everything".
        assert!(!inside(&root, &root.join("../new.txt")));
        assert!(!inside(&root, &root.join("a/../../new.txt")));
        assert!(!inside(&root, &base.join("work-evil/new.txt")));
        assert!(!inside(&root, &base.join("elsewhere/new.txt")));
    }

    /// The existing part of a path is spelled the way the filesystem spells it, so a root written
    /// in another case still holds a new file (a case-insensitive filesystem: Windows, macOS).
    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn a_new_file_in_the_root_is_inside_whatever_the_case() {
        let base = scratch("newcase");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();
        let upper = base.join("WORK");
        if !upper.exists() {
            eprintln!("skipped: this volume is case-sensitive");
            return;
        }
        assert!(inside(&root, &upper.join("new.txt")));
        assert!(inside(&upper, &root.join("new.txt")));
        assert!(!inside(&upper, &base.join("WORK-evil/new.txt")));
    }

    /// New files behind a junction are outside FOR THE RIGHT REASON: the same junction path with a
    /// real directory in its place is inside, so a refusal below is the link being followed, not the
    /// blanket "a path that does not exist is outside" that used to hide it.
    #[test]
    #[cfg(windows)]
    fn a_new_file_behind_a_junction_is_outside_and_one_in_a_real_directory_is_inside() {
        let base = scratch("newjunction");
        let root = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        junction(&root.join("link"), &outside);
        assert!(inside(&root, &root.join("real/nope/new.txt")), "control: a new file in a real directory");
        assert!(!inside(&root, &root.join("link/nope/new.txt")), "a new file behind a junction");
        assert!(!inside(&root, &root.join("link/new.txt")));
    }

    /// The same on Windows with the 8.3 spellings: a new file in the root reached by the root's short
    /// name is inside; a new file outside it by its short name is not.
    #[test]
    #[cfg(windows)]
    fn a_new_file_is_judged_the_same_in_short_and_long_spellings() {
        let base = scratch("newshort");
        let root = base.join("work_directory_with_a_long_name");
        let outside = base.join("outside_directory_with_a_long_name");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let (short_root, short_outside) = (short_path(&root), short_path(&outside));
        if short_root == root || short_outside == outside {
            skip("this volume does not make 8.3 short names");
            return;
        }
        assert!(inside(&root, &short_root.join("new.txt")));
        assert!(inside(&short_root, &root.join("new.txt")));
        assert!(inside(&short_root, &short_root.join("new.txt")));
        assert!(!inside(&root, &short_outside.join("new.txt")));
        assert!(!inside(&short_root, &outside.join("new.txt")));
    }


    // --- pb-20261111-71, the security bar on the new-file change ---------------------------------
    // Judging a path that does not exist yet by its canonical existing ancestor turns the new-file
    // paths from "always outside" into "decided by where they really are". These pin the escapes
    // that must stay outside (each one is a NEW file, so a lexical shortcut for new files would let
    // it through) and the verbatim spellings both sides must agree on.

    /// A root, an outside directory with a secret, and the long-named dirs the link kinds below use.
    #[cfg(windows)]
    fn link_fixture(tag: &str) -> (PathBuf, PathBuf) {
        let base = scratch(tag);
        let root = base.join("work_directory_with_a_long_name");
        let outside = base.join("outside_directory_with_a_long_name");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        (root, outside)
    }

    /// The four tests below take one kind of link each (so one regression names the kind that
    /// broke): every NEW file behind it is outside, with an in-root control. A lexical shortcut for
    /// a path that does not exist yet would let each of them through.
    #[test]
    #[cfg(windows)]
    fn a_new_file_behind_a_junction_by_its_long_name_is_outside() {
        let (root, outside) = link_fixture("nl-junction");
        let link = root.join("a_junction_with_a_long_name");
        junction(&link, &outside);
        assert!(!inside(&root, &link.join("new.txt")), "new file behind a junction");
        assert!(!inside(&root, &link.join("x/y/new.txt")), "new file, several missing levels, behind a junction");
        assert!(inside(&root, &root.join("a").join("new.txt")), "control: a new file in a real directory");
    }

    /// The junction reached by its 8.3 short name lies lexically inside the root; only resolving it
    /// shows it is not.
    #[test]
    #[cfg(windows)]
    fn a_new_file_behind_a_junction_named_by_its_8_3_alias_is_outside() {
        let (root, outside) = link_fixture("nl-alias");
        let link = root.join("a_junction_with_a_long_name");
        junction(&link, &outside);
        let short = short_path(&link);
        if short == link {
            skip("this volume does not make 8.3 short names");
            return;
        }
        // `short` shortens every component (the root's too), so it differs from the root as text. The
        // alias that matters keeps the root's long name and shortens only the junction's: lexically it
        // lies INSIDE the root, which is what a lexical shortcut would believe.
        let alias = root.join(short.file_name().unwrap());
        assert!(!inside(&root, &alias.join("new.txt")), "new file behind a junction named by its 8.3 alias (long root)");
        assert!(!inside(&root, &alias.join("secret.txt")), "existing file behind a junction named by its 8.3 alias (long root)");
        assert!(!inside(&root, &short.join("new.txt")), "new file behind a junction named by its 8.3 alias (all short)");
        assert!(!inside(&root, &short.join("secret.txt")), "existing file behind a junction named by its 8.3 alias (all short)");
        assert!(inside(&root, &short_path(&root).join("a").join("new.txt")), "control: a new file in the root by its short name");
    }

    #[test]
    #[cfg(windows)]
    fn a_new_file_behind_a_directory_symlink_is_outside() {
        let (root, outside) = link_fixture("nl-symlink");
        if let Err(e) = std::os::windows::fs::symlink_dir(&outside, root.join("symlink_dir")) {
            skip(&format!("cannot create a directory symlink here ({e})"));
            return;
        }
        assert!(!inside(&root, &root.join("symlink_dir/new.txt")), "new file behind a directory symlink");
        assert!(!inside(&root, &root.join("symlink_dir/x/new.txt")));
        assert!(inside(&root, &root.join("a").join("new.txt")), "control");
    }

    /// `..` written with mixed separators to a file that is not there yet climbs out all the same.
    #[test]
    #[cfg(windows)]
    fn a_new_file_through_mixed_separator_dotdot_is_outside() {
        let (root, _outside) = link_fixture("nl-mixed");
        let r = root.display();
        for escape in [format!("{r}\\a/..\\..\\new.txt"), format!("{r}/a\\../..\\new.txt"), format!("{r}\\..\\new.txt")] {
            assert!(!inside(&root, Path::new(&escape)), "mixed separators, new file: {escape}");
        }
        assert!(inside(&root, Path::new(&format!("{r}\\a/..\\a\\new.txt"))), "control: the same mixture that stays inside");
    }

    /// `std::fs::canonicalize` returns `\\?\C:\...`; a path written that way must be judged like the
    /// plain spelling, on both sides: an escaping one is outside, an in-root one is inside.
    #[test]
    #[cfg(windows)]
    fn verbatim_paths_are_judged_like_plain_ones() {
        let base = scratch("verbatim");
        let root = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("in.txt"), "x").unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        let verbatim = |p: &Path| PathBuf::from(format!(r"\\?\{}", p.display()));
        let (v_root, v_outside) = (verbatim(&root), verbatim(&outside));
        // outside, as an existing file and as a new one
        assert!(!inside(&root, &v_outside.join("secret.txt")), "verbatim, outside, existing");
        assert!(!inside(&root, &v_outside.join("new.txt")), "verbatim, outside, new");
        assert!(!inside(&root, &v_root.join("..").join("outside").join("new.txt")), "verbatim, `..` out of the root");
        assert!(!inside(&v_root, &outside.join("secret.txt")), "verbatim ROOT, plain outside path");
        // inside, existing and new, on either side
        assert!(inside(&root, &v_root.join("in.txt")), "verbatim, inside, existing");
        assert!(inside(&root, &v_root.join("new.txt")), "verbatim, inside, new");
        assert!(inside(&v_root, &root.join("in.txt")), "verbatim ROOT, plain inside path");
        assert!(inside(&v_root, &root.join("new.txt")), "verbatim ROOT, plain new inside path");
        assert!(inside(&v_root, &v_root.join("new.txt")));
    }

    /// A UNC path names another machine's share: outside, whatever it is called, and judging it must
    /// not hang. `localhost` and a share that does not exist answer at once.
    #[test]
    #[cfg(windows)]
    fn a_unc_path_is_outside() {
        let base = scratch("unc");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();
        let started = std::time::Instant::now();
        assert!(!inside(&root, Path::new(r"\\localhost\pb71_no_such_share\x.txt")));
        assert!(!inside(&root, Path::new(r"\\?\UNC\localhost\pb71_no_such_share\x.txt")));
        // The drive's own administrative share is the root's own place, spelled as UNC: it is not
        // judged inside (a different spelling is not trusted), so it asks first.
        let drive = base.display().to_string();
        let admin = format!(r"\\localhost\{}$\{}", &drive[..1], drive[3..].replace('/', "\\"));
        assert!(!inside(&root, &Path::new(&admin).join("work").join("x.txt")), "UNC to the root's own place: {admin}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "judging a UNC path took {:?}", started.elapsed());
    }


    // --- pb-20261111-71, found by the runner: a prefix must keep its drive and its share ----------

    /// A directory under the current directory, so it is on the current drive (what a root that is
    /// the session's working directory looks like), removed by the caller.
    #[cfg(windows)]
    fn root_on_the_current_drive(tag: &str) -> PathBuf {
        let root = std::env::current_dir().unwrap().join(format!("pb71-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("in.txt"), "x").unwrap();
        root
    }

    /// The bypass: a path on ANOTHER drive written `\\?\Y:\<the root's own tail>` was judged as
    /// `<current drive>:\<tail>`, i.e. inside the root. Both spellings of the other drive are outside.
    #[test]
    #[cfg(windows)]
    fn a_path_on_another_drive_is_outside_even_with_the_roots_own_tail() {
        let root = root_on_the_current_drive("drive");
        let root_text = root.display().to_string();
        let (own, tail) = root_text.split_at(2); // `D:` and `\a\b\...`
        let other = ('A'..='Z')
            .map(|l| l.to_string())
            .find(|l| !l.eq_ignore_ascii_case(&own[..1]) && Path::new(&format!("{l}:\\")).exists());
        let Some(other) = other else {
            let _ = std::fs::remove_dir_all(&root);
            skip("only one drive here, so there is no other drive to name");
            return;
        };
        for spelling in [format!(r"\\?\{other}:{tail}\in.txt"), format!(r"\\?\{other}:{tail}\new.txt"), format!("{other}:{tail}\\in.txt"), format!("{other}:{tail}\\new.txt")] {
            assert!(!inside(&root, Path::new(&spelling)), "another drive was judged inside the root: {spelling}");
        }
        // The controls: the same tail on the root's own drive, plain and verbatim, is inside.
        assert!(inside(&root, Path::new(&format!(r"\\?\{own}{tail}\in.txt"))), "verbatim, own drive, existing");
        assert!(inside(&root, Path::new(&format!(r"\\?\{own}{tail}\new.txt"))), "verbatim, own drive, new");
        assert!(inside(&root, &root.join("in.txt")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same for a UNC share: `\\server\share\<the root's own tail>` is another machine's file,
    /// not the root's, however the tail reads. `localhost` and a share that is not there answer at
    /// once; a guard keeps a slow lookup from hanging CI.
    #[test]
    #[cfg(windows)]
    fn a_unc_path_with_the_roots_own_tail_is_outside() {
        let root = root_on_the_current_drive("unc");
        let root_text = root.display().to_string();
        let tail = &root_text[2..];
        let admin = format!("{}$", &root_text[..1]);
        let started = std::time::Instant::now();
        for spelling in [
            // The bypass shape: the share is the root's own drive (`C$`), so dropping the UNC prefix
            // leaves exactly the root's tail on the current drive. Old `Step::Root` read these INSIDE.
            format!(r"\\localhost\{admin}{tail}\in.txt"),
            format!(r"\\localhost\{admin}{tail}\new.txt"),
            format!(r"\\?\UNC\localhost\{admin}{tail}\in.txt"),
            format!(r"\\localhost\pb71_no_such_share{tail}\in.txt"),
            format!(r"\\localhost\pb71_no_such_share{tail}\new.txt"),
            format!(r"\\?\UNC\localhost\pb71_no_such_share{tail}\in.txt"),
        ] {
            assert!(!inside(&root, Path::new(&spelling)), "a UNC path was judged inside the root: {spelling}");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(30), "judging UNC paths took {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Verbatim paths turn Win32 normalization off: `..` is a literal name, so the OS cannot open
    /// anything through it. Judging a path that climbs out of the root with `..` outside is the
    /// safe answer, and the two must agree: classification OUTSIDE and the real open fails.
    #[test]
    #[cfg(windows)]
    fn a_verbatim_path_with_dotdot_is_outside_and_cannot_be_opened() {
        let base = scratch("vdotdot");
        let root = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::fs::write(base.join("stolen.txt"), "s").unwrap();
        let v = |tail: &str| PathBuf::from(format!(r"\\?\{}\{tail}", root.display()));
        for escape in [v(r"..\outside\secret.txt"), v(r"a\..\..\stolen.txt"), v(r"..\stolen.txt")] {
            assert!(!inside(&root, &escape), "a verbatim `..` path was judged inside: {}", escape.display());
            assert!(std::fs::read(&escape).is_err(), "the OS reached outside through a verbatim `..`: {}", escape.display());
        }
        // A verbatim `..` that stays under the root is judged by where it lands.
        assert!(inside(&root, &v(r"a\..\a\new.txt")));
    }

    /// Win32 strips trailing dots and spaces from a segment; a verbatim path keeps them. Neither
    /// spelling may be used to step out: `evil. \..\..\x` and `evil.\..\..\x` climb out, a file
    /// named with a trailing dot or space in the root is a new file in the root.
    #[test]
    #[cfg(windows)]
    fn trailing_dots_and_spaces_cannot_hide_an_escape() {
        let base = scratch("trailing");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(base.join("stolen.txt"), "s").unwrap();
        let r = root.display();
        for escape in [
            format!(r"{r}\evil. \..\..\stolen.txt"),
            format!(r"{r}\evil.\..\..\stolen.txt"),
            format!(r"{r}\evil.\..\..\newfile.txt"),
            format!(r"\\?\{r}\evil. \..\..\stolen.txt"),
            format!(r"\\?\{r}\evil.\..\..\newfile.txt"),
        ] {
            assert!(!inside(&root, Path::new(&escape)), "trailing dot or space hid an escape: {escape}");
        }
        for new_file in [format!(r"{r}\new.txt."), format!(r"{r}\new.txt "), format!(r"\\?\{r}\new.txt."), format!(r"\\?\{r}\new.txt ")] {
            assert!(inside(&root, Path::new(&new_file)), "a new file with a trailing dot or space is in the root: {new_file}");
        }
    }

    /// The root is spelled by the filesystem in the canonical case; a path in another case (the
    /// volume is case-insensitive) is the same place, plain or verbatim, existing or new.
    #[test]
    #[cfg(windows)]
    fn another_case_than_the_roots_is_still_inside() {
        let base = scratch("mixedcase");
        let root = base.join("Work");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("In.txt"), "x").unwrap();
        let upper = root.display().to_string().to_uppercase();
        let lower = root.display().to_string().to_lowercase();
        for spelling in [
            format!(r"{upper}\IN.TXT"),
            format!(r"{lower}\in.txt"),
            format!(r"{upper}\NEW.TXT"),
            format!(r"\\?\{upper}\IN.TXT"),
            format!(r"\\?\{lower}\new.txt"),
        ] {
            assert!(inside(&root, Path::new(&spelling)), "another case was judged outside: {spelling}");
        }
        // and it still is not a license to leave: a sibling in any case is outside
        assert!(!inside(&root, Path::new(&format!(r"{upper}-EVIL\NEW.TXT"))));
        assert!(!inside(&root, Path::new(&format!(r"{upper}\..\OUTSIDE.TXT"))));
    }


    // --- pb-20261111-76: the security bar on prefixes, devices and other machines' shares ---------

    /// Prefixes compare by what they name. Pure (no filesystem), Windows only because only Windows
    /// parses a prefix.
    #[test]
    #[cfg(windows)]
    fn prefixes_compare_by_what_they_name() {
        let k = |s: &str| prefix_key(Path::new(s));
        assert_eq!(k(r"C:\x"), k(r"\\?\c:\y"), "a drive, plain or verbatim, any case");
        assert_ne!(k(r"C:\x"), k(r"D:\x"));
        assert_eq!(k(r"\\Server\Share\x"), k(r"\\?\UNC\server\SHARE\y"), "a share, plain or verbatim, any case");
        assert_ne!(k(r"\\server\share\x"), k(r"\\server\other\x"));
        assert_ne!(k(r"\\server\share\x"), k(r"\\other\share\x"));
        assert_ne!(k(r"\\server\share\x"), k(r"C:\x"));
        for namespace in [
            r"\\.\C:\x",
            r"\\.\pipe\x",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\y",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x",
        ] {
            assert!(k(namespace).starts_with("other:"), "{namespace} -> {}", k(namespace));
            assert_ne!(k(namespace), k(r"C:\x"));
        }
        // May a walk go through it? A local drive, or the root's own share: nothing else.
        let allowed = |prefix: &str, own: &str| prefix_allowed(std::ffi::OsStr::new(prefix), own);
        assert!(allowed(r"C:", "disk:D"), "any local drive");
        assert!(allowed(r"\\?\D:", ""));
        assert!(allowed(r"\\SRV\SHARE", r"unc:srv\share"), "the root's own share");
        assert!(allowed(r"\\?\UNC\srv\share", r"unc:srv\share"), "the root's own share, verbatim");
        assert!(!allowed(r"\\srv\other", r"unc:srv\share"), "another share of the same server");
        assert!(!allowed(r"\\attacker\share", r"unc:srv\share"));
        assert!(!allowed(r"\\attacker\share", "disk:C"), "a UNC path when the root is on a drive");
        for namespace in [r"\\.\C:", r"\\.\pipe", r"\\?\GLOBALROOT", r"\\?\Volume{00000000-0000-0000-0000-000000000000}"] {
            assert!(!allowed(namespace, "disk:C"), "{namespace}");
            assert!(!allowed(namespace, "other:x"), "{namespace}");
        }
    }

    /// Device and volume namespaces are outside, and judging them opens none of them.
    #[test]
    #[cfg(windows)]
    fn device_and_volume_namespaces_are_outside() {
        let root = root_on_the_current_drive("devices");
        let root_text = root.display().to_string();
        let started = std::time::Instant::now();
        for spelling in [
            r"\\.\C:\Windows\System32\drivers\etc\hosts".to_owned(),
            r"\\.\pipe\pb71_no_such_pipe".to_owned(),
            r"\\.\PhysicalDrive0".to_owned(),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\Windows".to_owned(),
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\a\b".to_owned(),
            // the root's own file through the device namespace is not trusted either (it asks first)
            format!(r"\\.\{root_text}\in.txt"),
        ] {
            assert!(!inside(&root, Path::new(&spelling)), "a device or volume path was judged inside: {spelling}");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "judging devices took {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Another machine's share is denied BEFORE any filesystem call, so judging it never makes
    /// Windows connect to it (and send it the user's credentials): a non-routable server would take
    /// seconds to time out, so the answer must come back in under one.
    #[test]
    #[cfg(windows)]
    fn another_machines_share_is_denied_without_a_network_attempt() {
        let root = root_on_the_current_drive("network");
        let root_text = root.display().to_string();
        let tail = &root_text[2..];
        let started = std::time::Instant::now();
        for spelling in [
            r"\\192.0.2.1\share\x.txt".to_owned(),
            r"\\?\UNC\192.0.2.1\share\x.txt".to_owned(),
            format!(r"\\192.0.2.1\share{tail}\in.txt"),
            format!(r"\\192.0.2.1\c$\{}", &root_text[3..]),
        ] {
            assert!(!inside(&root, Path::new(&spelling)), "another machine's share was judged inside: {spelling}");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "judging a UNC path took {:?}: a network attempt", started.elapsed());

        // The same through a link: a directory symlink in the root whose target is that share.
        match std::os::windows::fs::symlink_dir(r"\\192.0.2.1\share", root.join("to_share")) {
            Ok(()) => {
                let started = std::time::Instant::now();
                assert!(!inside(&root, &root.join("to_share").join("x.txt")), "a link to another machine's share was judged inside");
                assert!(!inside(&root, &root.join("to_share")));
                assert!(started.elapsed() < std::time::Duration::from_secs(1), "following a link to a UNC share took {:?}", started.elapsed());
            }
            Err(e) => skip(&format!("cannot create a directory symlink here ({e})")),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link whose target is another machine's share is refused like the share written out, from the
    /// link's own text, before anything is opened through it; a link that stays on the machine is not.
    #[test]
    #[cfg(windows)]
    fn a_link_to_another_machines_share_is_a_foreign_target() {
        let root = root_on_the_current_drive("link-target");
        std::fs::create_dir_all(root.join("src")).unwrap();
        if let Err(e) = std::os::windows::fs::symlink_dir(r"\\192.0.2.1\share", root.join("to_share")) {
            skip(&format!("cannot create a directory symlink here ({e})"));
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        std::os::windows::fs::symlink_dir(root.join("to_share"), root.join("hop")).unwrap();
        std::os::windows::fs::symlink_dir(root.join("src"), root.join("local")).unwrap();
        let started = std::time::Instant::now();
        for p in [root.join("to_share"), root.join("to_share").join("x.txt"), root.join("hop"), root.join("hop").join("a").join("b.txt")] {
            assert!(foreign_target(&root, &p), "a link to another machine's share was not refused: {}", p.display());
        }
        assert!(foreign_target(&root, Path::new("to_share")), "the relative spelling");
        for p in [root.join("local"), root.join("local").join("a.txt"), root.join("src").join("a.txt"), root.join("new.txt")] {
            assert!(!foreign_target(&root, &p), "a path that stays on this machine was refused: {}", p.display());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "judging links took {:?}: a network attempt", started.elapsed());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A chain of links longer than the hop limit is REFUSED, never let through to be followed by the
    /// filesystem, and so is a pair of links that point at each other. The chains end on this machine,
    /// so it is the limit, not a share, that refuses them.
    #[test]
    #[cfg(windows)]
    fn a_chain_of_links_past_the_hop_limit_and_a_cycle_are_refused() {
        let root = root_on_the_current_drive("link-hops");
        std::fs::create_dir_all(root.join("end")).unwrap();
        let chain = |n: usize| {
            // l1 -> l2 -> ... -> l<n> -> end
            for i in 1..=n {
                let target = if i == n { root.join("end") } else { root.join(format!("c{n}_l{}", i + 1)) };
                std::os::windows::fs::symlink_dir(target, root.join(format!("c{n}_l{i}"))).unwrap();
            }
            root.join(format!("c{n}_l1")).join("x.txt")
        };
        if let Err(e) = std::os::windows::fs::symlink_dir(root.join("end"), root.join("probe")) {
            skip(&format!("cannot create a directory symlink here ({e})"));
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let at_the_limit = chain(LINK_HOPS);
        assert!(!foreign_target(&root, &at_the_limit), "{LINK_HOPS} links on this machine are within the limit");
        let past_the_limit = chain(LINK_HOPS + 1);
        assert!(foreign_target(&root, &past_the_limit), "{} links were not refused", LINK_HOPS + 1);
        // Two links that point at each other.
        std::os::windows::fs::symlink_dir(root.join("cycle_b"), root.join("cycle_a")).unwrap();
        std::os::windows::fs::symlink_dir(root.join("cycle_a"), root.join("cycle_b")).unwrap();
        assert!(foreign_target(&root, &root.join("cycle_a").join("x.txt")), "a cycle of links was not refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Links that point at each other are not followed forever and not left for the canonicalization
    /// to follow: the path is not judged inside.
    #[test]
    #[cfg(unix)]
    fn links_that_point_at_each_other_are_not_inside() {
        let base = scratch("loop");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("a")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("b")).unwrap();
        assert!(!inside(&root, &root.join("a/x.txt")));
        assert!(inside(&root, &root.join("ok.txt")), "the control");
    }


    /// `foreign_location` reads only the text of a path, whatever the separators.
    #[test]
    #[cfg(windows)]
    fn foreign_location_is_lexical_and_catches_every_spelling() {
        let cwd = Path::new(r"C:\Users\x\proj");
        let foreign = |s: &str| foreign_location(cwd, Path::new(s));
        for spelling in [
            r"\\203.0.113.1\s\x",
            "//203.0.113.1/s/x",
            r"\\?\UNC\203.0.113.1\s\x",
            r"\\localhost\C$\Users\x\proj\a.txt",
            r"\\.\C:\x",
            r"\\.\pipe\x",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x",
        ] {
            assert!(foreign(spelling), "not caught: {spelling}");
        }
        for local in [r"C:\Users\x\proj\a.txt", r"D:\other", r"\\?\C:\Users\x\proj\a.txt", r"a\b.txt", "a/b.txt", r"..\x"] {
            assert!(!foreign(local), "wrongly caught: {local}");
        }
        // A root that is itself on a share: that share is its own, any other is foreign.
        let unc_cwd = Path::new(r"\\srv\share\proj");
        assert!(!foreign_location(unc_cwd, Path::new(r"\\SRV\share\proj\x")));
        assert!(!foreign_location(unc_cwd, Path::new(r"\\?\UNC\srv\SHARE\proj\x")));
        assert!(foreign_location(unc_cwd, Path::new(r"\\srv\other\x")));
        assert!(foreign_location(unc_cwd, Path::new(r"\\attacker\share\proj\x")));
    }


    /// Windows reads a leading pair of ANY slash mix as UNC, and a leading `\??\` as the NT object
    /// namespace: each spelling is caught, and `localhost`, `127.0.0.1` and the machine's own name
    /// are foreign like any other host.
    #[test]
    #[cfg(windows)]
    fn every_windows_unc_spelling_is_caught() {
        let cwd = Path::new(r"C:\Users\x\proj");
        let own_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "THIS-MACHINE".into());
        let mut spellings: Vec<String> = [
            r"\\203.0.113.1\s\x",
            "//203.0.113.1/s/x",
            r"\/203.0.113.1/s/x",
            r"/\203.0.113.1\s\x",
            r"\\203.0.113.1/s\x",
            r"\\?\UNC\203.0.113.1\s\x",
            r"\\.\UNC\203.0.113.1\s\x",
            r"\??\UNC\203.0.113.1\s\x",
            r"\??\C:\Users\x\proj\a.txt",
            r"\\203.0.113.1",
            r"\\203.0.113.1\s",
            r"\\localhost\C$\Users\x\proj\a.txt",
            "//localhost/C$/Users/x/proj/a.txt",
            r"\\127.0.0.1\C$\Users\x\proj\a.txt",
            r"\\[::1]\C$\x",
            r"\\.\C:\Users\x\proj\a.txt",
            r"\\.\pipe\x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        spellings.push(format!(r"\\{own_name}\C$\Users\x\proj\a.txt"));
        spellings.push(format!("//{own_name}/C$/Users/x/proj/a.txt"));
        for s in &spellings {
            assert!(foreign_location(cwd, Path::new(s)), "not caught: {s}");
        }
        // What stays local: plain and verbatim drives in either slash, relative paths.
        for local in [r"C:\x", "C:/x/y", r"\\?\C:\x", r"\\?\c:\x\y", r"d:\a\b", "a/b", r"a\b", r"\x"] {
            assert!(!foreign_location(cwd, Path::new(local)), "wrongly caught: {local}");
        }
    }

}
