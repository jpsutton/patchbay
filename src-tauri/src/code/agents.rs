//! Coding agent backends for the Code tab.
//!
//! Every backend speaks ACP over stdio, so `acp.rs` drives them all the same way. This module
//! holds what differs between them: how to find and launch the agent, its environment, how
//! Patchbay's custom instructions reach it, and which non-standard extensions it understands.
//! The UI branches on [`Extensions`], never on the agent's identity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::grok_config;
use crate::db::{CodeSession, Db};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Grok,
    ClaudeCode,
    Opencode,
    Cursor,
    Kilo,
}

/// The Claude Code ACP adapter, run through `npx` when it isn't installed.
pub const CLAUDE_ACP_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp@0.93.0";

impl AgentKind {
    pub const ALL: [AgentKind; 5] = [
        AgentKind::Grok,
        AgentKind::ClaudeCode,
        AgentKind::Opencode,
        AgentKind::Cursor,
        AgentKind::Kilo,
    ];

    /// The id stored in `code_sessions.agent` and preferences.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Grok => "grok",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Opencode => "opencode",
            AgentKind::Cursor => "cursor",
            AgentKind::Kilo => "kilo",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Grok => "Grok Build",
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Opencode => "OpenCode",
            AgentKind::Cursor => "Cursor CLI",
            AgentKind::Kilo => "Kilo Code",
        }
    }

    /// Executables to look for on PATH, in order, and the arguments that start ACP on stdio.
    fn program(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            AgentKind::Grok => (&[], &[]),
            AgentKind::ClaudeCode => (&["claude-agent-acp"], &[]),
            AgentKind::Opencode => (&["opencode"], &["acp"]),
            AgentKind::Cursor => (&["cursor-agent"], &["acp"]),
            AgentKind::Kilo => (&["kilo", "kilocode"], &["acp"]),
        }
    }

    /// True when the agent runs on Patchbay's providers and models; the others bring their own
    /// sign-in and model list.
    pub fn uses_patchbay_models(self) -> bool {
        self == AgentKind::Grok
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            AgentKind::Grok => "Bundled with Patchbay; in a dev checkout run scripts/build-sidecar.sh.",
            AgentKind::ClaudeCode => {
                "Install Node.js (Patchbay then runs the adapter through npx), or `npm install -g @agentclientprotocol/claude-agent-acp`. Sign in with `claude`."
            }
            AgentKind::Opencode => "Install from https://opencode.ai, then run `opencode auth login`.",
            AgentKind::Cursor => "Install from https://cursor.com/cli, then run `cursor-agent login`.",
            AgentKind::Kilo => "`npm install -g @kilocode/cli`, then run `kilo auth login`.",
        }
    }

    /// How to sign in, for an `auth_required` error.
    pub fn login_hint(self) -> &'static str {
        match self {
            AgentKind::Grok => "Add a provider and model in Settings.",
            AgentKind::ClaudeCode => "Run `claude` in a terminal and sign in with /login.",
            AgentKind::Opencode => "Run `opencode auth login` in a terminal.",
            AgentKind::Cursor => "Run `cursor-agent login` in a terminal.",
            AgentKind::Kilo => "Run `kilo auth login` in a terminal.",
        }
    }
}

/// Non-standard ACP extensions an agent understands. Sent to the UI with `sessionReady`.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Extensions {
    /// Grok Build's `x.ai/statusLine` client capability (`session_status` usage updates).
    pub status_line: bool,
    /// Grok Build's `x.ai/rewind/*` methods, behind "edit and resend".
    pub rewind: bool,
    /// Runs on Patchbay's providers and models, so a change to them needs a restart to apply.
    pub patchbay_models: bool,
}

/// What makes an agent ask Patchbay before acting, so the session's approval mode decides
/// (code/permissions.rs, docs/design/approval-modes.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalSetup {
    /// Agent mode to select once the session is up, and again whenever the agent leaves it for
    /// one in `hide_modes`.
    pub select_mode: Option<String>,
    /// Agent modes that are really approval policies of the agent's own; hidden from the UI.
    pub hide_modes: &'static [&'static str],
    /// What this agent does without asking, whatever the mode. Shown in the session header.
    pub gaps: &'static [&'static str],
}

impl ApprovalSetup {
    fn for_agent(kind: AgentKind) -> Self {
        match kind {
            // Grok's own mode is pinned to "default" in its config.toml (grok_config.rs).
            AgentKind::Grok | AgentKind::Opencode | AgentKind::Kilo => Self::default(),
            AgentKind::ClaudeCode => Self {
                select_mode: Some("default".into()),
                hide_modes: &["acceptEdits", "auto", "bypassPermissions", "dontAsk"],
                gaps: &["Rules in your own Claude Code settings (permissions.allow) run without asking."],
            },
            // Cursor has no per-session approval control over ACP.
            AgentKind::Cursor => Self {
                select_mode: None,
                hide_modes: &[],
                gaps: &[
                    "Cursor edits files in this project without asking, in every mode.",
                    "Shell commands on Cursor's own allowlist (~/.cursor/cli-config.json) run without asking.",
                ],
            },
        }
    }
}

/// Config that makes OpenCode and Kilo ask over ACP for every tool. Both read inline config
/// from an env var; deny rules in the user's own config still apply.
const ASK_EVERYTHING: &str = r#"{"permission":{"*":"ask"}}"#;

/// Where an agent's executable was found.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The path set in Settings.
    Override,
    /// Next to Patchbay's own executable, or the dev-staged sidecar.
    Bundled,
    Path,
    /// `npx`, which downloads the package on first use.
    Npx,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub source: Source,
}

/// Everything `acp::spawn` needs to start one agent process.
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub ext: Extensions,
    /// `_meta` for `session/new` and `session/load`.
    pub session_meta: Value,
    /// Agent-native model id to select once the session is up (non-Patchbay agents).
    pub initial_model: Option<String>,
    /// How to sign in, for an `auth_required` error.
    pub login_hint: &'static str,
    pub approvals: ApprovalSetup,
}

/// User overrides from Settings: agent id -> executable path.
pub type AgentPaths = BTreeMap<String, String>;

/// PATH plus the user-level bin directories a desktop launch may leave out of it.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        for d in [".local/bin", "bin", ".npm-global/bin", ".bun/bin", ".opencode/bin", ".cursor/bin", ".volta/bin"] {
            dirs.push(home.join(d));
        }
    }
    if cfg!(unix) {
        dirs.push("/usr/local/bin".into());
        dirs.push("/opt/homebrew/bin".into());
    }
    dirs
}

/// PATH for an agent process: the program's own directory first (an npm shim's `node` usually
/// sits beside it), then [`search_dirs`]. Without this, a shim found in `~/.npm-global/bin`
/// fails with "env: node: No such file" when Patchbay was started from a desktop entry.
fn child_path(program: &Path) -> Option<String> {
    let mut dirs = Vec::new();
    if let Some(dir) = program.parent().filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(dir.to_path_buf());
    }
    for d in search_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Locate an executable on [`search_dirs`].
pub fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    for dir in search_dirs() {
        for ext in exts {
            let p = dir.join(format!("{name}{ext}"));
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Find the agent's executable: the Settings override, then the bundled copy (Grok), PATH, and
/// for Claude Code a fallback to `npx`.
pub fn resolve(kind: AgentKind, overrides: &AgentPaths) -> Result<Resolved> {
    let (names, acp_args) = kind.program();
    let args: Vec<String> = acp_args.iter().map(|s| (*s).to_owned()).collect();
    if let Some(p) = overrides.get(kind.id()).map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let program = PathBuf::from(p);
        if !program.is_file() {
            return Err(anyhow!("{} path {p} does not exist", kind.label()));
        }
        return Ok(Resolved { program, args, source: Source::Override });
    }
    if kind == AgentKind::Grok {
        return super::sidecar_path().map(|program| Resolved { program, args, source: Source::Bundled });
    }
    if let Some(program) = names.iter().find_map(|n| which(n)) {
        return Ok(Resolved { program, args, source: Source::Path });
    }
    if kind == AgentKind::ClaudeCode
        && let Some(npx) = which("npx")
    {
        return Ok(Resolved {
            program: npx,
            args: vec!["-y".into(), CLAUDE_ACP_PACKAGE.into()],
            source: Source::Npx,
        });
    }
    Err(anyhow!("{} not found. {}", kind.label(), kind.install_hint()))
}

/// What the Agents settings page shows for one backend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub available: bool,
    pub program: Option<String>,
    pub source: Option<Source>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub install_hint: &'static str,
    pub uses_patchbay_models: bool,
    /// Receives Settings → Code → custom instructions.
    pub supports_instructions: bool,
}

/// Probe every backend. Blocking: runs `--version` on each agent found, in parallel.
pub fn detect_all(overrides: &AgentPaths) -> Vec<AgentInfo> {
    std::thread::scope(|scope| {
        let probes: Vec<_> = AgentKind::ALL
            .into_iter()
            .map(|kind| scope.spawn(move || detect(kind, overrides)))
            .collect();
        probes.into_iter().filter_map(|p| p.join().ok()).collect()
    })
}

fn detect(kind: AgentKind, overrides: &AgentPaths) -> AgentInfo {
    let mut info = AgentInfo {
        id: kind.id(),
        label: kind.label(),
        available: false,
        program: None,
        source: None,
        version: None,
        error: None,
        install_hint: kind.install_hint(),
        uses_patchbay_models: kind.uses_patchbay_models(),
        supports_instructions: supports_instructions(kind),
    };
    match resolve(kind, overrides) {
        Ok(r) => {
            info.available = true;
            info.program = Some(r.program.to_string_lossy().into_owned());
            // `npx` would download the package just to print a version.
            if r.source != Source::Npx {
                info.version = version_of(&r.program);
            }
            info.source = Some(r.source);
        }
        Err(e) => info.error = Some(format!("{e:#}")),
    }
    info
}

fn version_of(program: &Path) -> Option<String> {
    let mut child = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let t0 = std::time::Instant::now();
    while child.try_wait().ok()?.is_none() {
        if t0.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let line = out.lines().find(|l| !l.trim().is_empty())?.trim();
    Some(line.chars().take(80).collect())
}

/// Instructions appended to the system prompt, in the form this agent reads them.
fn session_meta(kind: AgentKind, rules: &str) -> Value {
    let rules = rules.trim();
    if rules.is_empty() {
        return json!({});
    }
    match kind {
        AgentKind::Grok => json!({ "rules": rules }),
        // claude-agent-acp merges this into the SDK's `claude_code` preset prompt.
        AgentKind::ClaudeCode => json!({ "systemPrompt": { "append": rules } }),
        _ => json!({}),
    }
}

/// True when this agent receives Settings → Code → custom instructions.
pub fn supports_instructions(kind: AgentKind) -> bool {
    matches!(kind, AgentKind::Grok | AgentKind::ClaudeCode)
}

/// Build the launch for a session's agent: executable, arguments, environment and config.
pub fn prepare(db: &Db, data_dir: &Path, session: &CodeSession) -> Result<Launch> {
    let kind = AgentKind::parse(&session.agent)
        .ok_or_else(|| anyhow!("unknown coding agent {:?}", session.agent))?;
    let prefs = crate::prefs::load(db);
    let resolved = resolve(kind, &prefs.agent_paths)?;
    let session_meta = session_meta(kind, &prefs.agent_instructions);
    if kind == AgentKind::Grok {
        return prepare_grok(db, data_dir, session, resolved, session_meta);
    }
    let mut env = Vec::new();
    if let Some(path) = child_path(&resolved.program) {
        env.push(("PATH".to_owned(), path));
    }
    match kind {
        AgentKind::Opencode => {
            env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()));
            env.push(("OPENCODE_CONFIG_CONTENT".to_owned(), ASK_EVERYTHING.to_owned()));
        }
        AgentKind::Kilo => env.push(("KILO_CONFIG_CONTENT".to_owned(), ASK_EVERYTHING.to_owned())),
        _ => {}
    }
    Ok(Launch {
        program: resolved.program,
        args: resolved.args,
        env,
        env_remove: Vec::new(),
        ext: Extensions::default(),
        session_meta,
        initial_model: session.agent_model.clone(),
        login_hint: kind.login_hint(),
        approvals: ApprovalSetup::for_agent(kind),
    })
}

fn prepare_grok(
    db: &Db,
    data_dir: &Path,
    session: &CodeSession,
    resolved: Resolved,
    session_meta: Value,
) -> Result<Launch> {
    if db.list_models()?.is_empty() {
        return Err(anyhow!("add a provider and model in Settings first"));
    }
    let grok_home = data_dir.join("grok");
    let rendered = grok_config::render(db, session.model_ref.as_deref())?;
    grok_config::write(&grok_home, &rendered)?;
    let model_key = session
        .model_ref
        .as_deref()
        .filter(|r| db.get_model(r).ok().flatten().is_some())
        .map(grok_config::model_key);
    let mut args = vec!["agent".to_owned(), "--no-leader".to_owned()];
    if let Some(m) = model_key {
        args.push("-m".into());
        args.push(m);
    }
    args.push("stdio".into());
    let mut env: Vec<(String, String)> = [
        ("GROK_HOME", grok_home.to_string_lossy().as_ref()),
        ("GROK_TELEMETRY_ENABLED", "false"),
        ("GROK_TELEMETRY_MIXPANEL_ENABLED", "false"),
        ("GROK_TELEMETRY_TRACE_UPLOAD", "false"),
        ("GROK_DISABLE_AUTOUPDATER", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    env.extend(rendered.env);
    Ok(Launch {
        program: resolved.program,
        args,
        env,
        env_remove: vec!["XAI_API_KEY".into(), "GROK_CODE_XAI_API_KEY".into()],
        ext: Extensions { status_line: true, rewind: true, patchbay_models: true },
        session_meta,
        initial_model: None,
        login_hint: "",
        approvals: ApprovalSetup::for_agent(AgentKind::Grok),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::parse(k.id()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.id()));
        }
        assert_eq!(AgentKind::parse("nope"), None);
    }

    #[test]
    fn instructions_reach_agents_that_read_them() {
        assert_eq!(session_meta(AgentKind::Grok, " be terse "), json!({ "rules": "be terse" }));
        assert_eq!(
            session_meta(AgentKind::ClaudeCode, "be terse"),
            json!({ "systemPrompt": { "append": "be terse" } })
        );
        assert_eq!(session_meta(AgentKind::Opencode, "be terse"), json!({}));
        assert_eq!(session_meta(AgentKind::ClaudeCode, "  "), json!({}));
    }

    #[test]
    fn override_path_must_exist() {
        let mut o = AgentPaths::new();
        o.insert("opencode".into(), "/definitely/not/here".into());
        assert!(resolve(AgentKind::Opencode, &o).is_err());
        let exe = std::env::current_exe().unwrap();
        o.insert("opencode".into(), exe.to_string_lossy().into_owned());
        let r = resolve(AgentKind::Opencode, &o).unwrap();
        assert_eq!(r.source, Source::Override);
        assert_eq!(r.args, vec!["acp".to_owned()]);
    }
}
