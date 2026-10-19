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

use super::{agent_providers, grok_config};
use crate::db::{CodeSession, Db};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Grok,
    ClaudeCode,
    Opencode,
    Cursor,
    Kilo,
    Codex,
    Gemini,
    Antigravity,
}

/// The Claude Code ACP adapter, run through `npx` when it isn't installed.
pub const CLAUDE_ACP_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp@0.93.0";
/// The Codex ACP adapter, which brings its own Codex binary.
pub const CODEX_ACP_PACKAGE: &str = "@agentclientprotocol/codex-acp@2.2.2";
pub const GEMINI_CLI_PACKAGE: &str = "@google/gemini-cli@0.64.0";

impl AgentKind {
    pub const ALL: [AgentKind; 8] = [
        AgentKind::Grok,
        AgentKind::ClaudeCode,
        AgentKind::Opencode,
        AgentKind::Cursor,
        AgentKind::Kilo,
        AgentKind::Codex,
        AgentKind::Gemini,
        AgentKind::Antigravity,
    ];

    /// The id stored in `code_sessions.agent` and preferences.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Grok => "grok",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Opencode => "opencode",
            AgentKind::Cursor => "cursor",
            AgentKind::Kilo => "kilo",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::Antigravity => "antigravity",
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
            AgentKind::Codex => "Codex",
            AgentKind::Gemini => "Gemini CLI",
            AgentKind::Antigravity => "Antigravity",
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
            AgentKind::Codex => (&["codex-acp"], &[]),
            AgentKind::Gemini => (&["gemini"], &["--acp"]),
            // Google ships the ACP server only as an archive (see the ACP registry), so it is
            // rarely on PATH; Settings takes its path. The registry passes `--uid=` on Linux.
            AgentKind::Antigravity => (
                &["agy_acp_server", "agy_acp_server.par", "agy-acp-server"],
                if cfg!(target_os = "linux") { &["--uid="] } else { &[] },
            ),
        }
    }

    /// The npm package `resolve` falls back to through `npx`, with its ACP arguments.
    fn npx_package(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            AgentKind::ClaudeCode => Some((CLAUDE_ACP_PACKAGE, &[])),
            AgentKind::Codex => Some((CODEX_ACP_PACKAGE, &[])),
            AgentKind::Gemini => Some((GEMINI_CLI_PACKAGE, &["--acp"])),
            _ => None,
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
            AgentKind::Codex => {
                "Install Node.js (Patchbay then runs the adapter through npx), or `npm install -g @agentclientprotocol/codex-acp`. Sign in with `codex login`, or set OPENAI_API_KEY."
            }
            AgentKind::Gemini => {
                "Install Node.js (Patchbay then runs it through npx), or `npm install -g @google/gemini-cli`. Sign in by running `gemini`, or set GEMINI_API_KEY."
            }
            AgentKind::Antigravity => {
                "Download Google's Antigravity ACP server for your platform (listed in the ACP registry at agentclientprotocol.com), unpack it, and set the path to agy_acp_server in Settings."
            }
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
            AgentKind::Codex => "Run `codex login` in a terminal (`npm install -g @openai/codex` gets it), or set OPENAI_API_KEY.",
            AgentKind::Gemini => "Run `gemini` in a terminal and choose a sign-in method, or set GEMINI_API_KEY.",
            AgentKind::Antigravity => {
                "Set `auth.type` in ~/.gemini/antigravity-acp/settings.json (for example `gemini-api-key` with GEMINI_API_KEY set), or sign in from an editor that runs the Antigravity ACP server."
            }
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
            // Codex's modes are approval and sandbox presets. Read-only asks before every edit
            // and network use; its sandbox still runs shell commands without asking.
            AgentKind::Codex => Self {
                select_mode: Some(CODEX_ASKING_MODE.into()),
                hide_modes: &["workspace-write", "agent", "agent-full-access"],
                gaps: &[
                    "Codex runs shell commands inside its read-only sandbox without asking.",
                    "Rules in your own Codex config (~/.codex) may let more run without asking.",
                ],
            },
            AgentKind::Gemini => Self {
                select_mode: Some("default".into()),
                hide_modes: &["autoEdit", "yolo"],
                gaps: &[
                    "Gemini CLI reads, lists and searches files, and searches the web, without asking.",
                    "Allow rules in your own Gemini CLI settings and policies run without asking.",
                ],
            },
            AgentKind::Antigravity => Self {
                select_mode: Some("default".into()),
                hide_modes: &["auto_edit", "yolo"],
                gaps: &["Antigravity reads and searches files without asking."],
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

/// Codex mode that asks before edits; also its starting mode (`INITIAL_AGENT_MODE`).
const CODEX_ASKING_MODE: &str = "read-only";

/// Config that makes OpenCode and Kilo ask over ACP for every tool. Both read inline config
/// from an env var; deny rules in the user's own config still apply.
fn ask_everything() -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([("permission".to_owned(), json!({ "*": "ask" }))])
}

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
/// for the agents published on npm a fallback to `npx`.
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
    if let Some((package, acp_args)) = kind.npx_package()
        && let Some(npx) = which("npx")
    {
        let mut args = vec!["-y".to_owned(), package.to_owned()];
        args.extend(acp_args.iter().map(|s| (*s).to_owned()));
        return Ok(Resolved { program: npx, args, source: Source::Npx });
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
    /// Can be set to run on Patchbay's providers instead of its own sign-in.
    pub takes_providers: bool,
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
        takes_providers: agent_providers::supported(kind),
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
    let mut meta = serde_json::Map::new();
    if !rules.is_empty() {
        match kind {
            AgentKind::Grok => {
                meta.insert("rules".into(), json!(rules));
            }
            // claude-agent-acp merges this into the SDK's `claude_code` preset prompt.
            AgentKind::ClaudeCode => {
                meta.insert("systemPrompt".into(), json!({ "append": rules }));
            }
            // The server advertises `_meta.antigravity.systemPrompt.append` in `initialize`.
            AgentKind::Antigravity => {
                meta.insert("antigravity".into(), json!({ "systemPrompt": { "append": rules } }));
            }
            // Codex takes them through `CODEX_CONFIG` (see `prepare`).
            _ => {}
        }
    }
    if kind == AgentKind::ClaudeCode {
        // The Claude Agent SDK streams signature-only thinking by default, so thought
        // chunks arrive with empty text and the ACP adapter drops them. Asking for
        // summarized display makes live `agent_thought_chunk` text reach the UI.
        meta.insert(
            "claudeCode".into(),
            json!({ "options": { "thinking": { "type": "adaptive", "display": "summarized" } } }),
        );
    }
    Value::Object(meta)
}

/// True when this agent receives Settings → Code → custom instructions.
pub fn supports_instructions(kind: AgentKind) -> bool {
    matches!(kind, AgentKind::Grok | AgentKind::ClaudeCode | AgentKind::Codex | AgentKind::Antigravity)
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
    // Patchbay's providers, only for an agent the user set to use them.
    let providers = match prefs.agent_providers.get(kind.id()).filter(|_| agent_providers::supported(kind)) {
        Some(model_ref) => agent_providers::setup(db, kind, model_ref)?,
        None => agent_providers::Setup::default(),
    };
    let mut config = ask_everything();
    config.extend(providers.config);
    let config = Value::Object(config).to_string();
    match kind {
        AgentKind::Opencode => {
            env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()));
            env.push(("OPENCODE_CONFIG_CONTENT".to_owned(), config));
        }
        AgentKind::Kilo => env.push(("KILO_CONFIG_CONTENT".to_owned(), config)),
        AgentKind::Codex => {
            env.push(("INITIAL_AGENT_MODE".to_owned(), CODEX_ASKING_MODE.to_owned()));
            // The adapter reads an API key from the environment only when told to sign in with
            // one; without a login it otherwise stops at `auth_required`.
            if ["CODEX_API_KEY", "OPENAI_API_KEY"]
                .iter()
                .any(|v| std::env::var(v).is_ok_and(|k| !k.trim().is_empty()))
            {
                env.push(("DEFAULT_AUTH_REQUEST".to_owned(), json!({ "methodId": "api-key" }).to_string()));
            }
            // Merged into every thread's config; `developer_instructions` joins Codex's prompt.
            let rules = prefs.agent_instructions.trim();
            if !rules.is_empty() {
                env.push(("CODEX_CONFIG".to_owned(), json!({ "developer_instructions": rules }).to_string()));
            }
        }
        _ => {}
    }
    env.extend(providers.env);
    Ok(Launch {
        program: resolved.program,
        args: resolved.args,
        env,
        env_remove: providers.env_remove,
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
        let claude = session_meta(AgentKind::ClaudeCode, "be terse");
        assert_eq!(claude["systemPrompt"], json!({ "append": "be terse" }));
        assert_eq!(
            claude["claudeCode"]["options"]["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(session_meta(AgentKind::Opencode, "be terse"), json!({}));
        assert_eq!(
            session_meta(AgentKind::Antigravity, "be terse"),
            json!({ "antigravity": { "systemPrompt": { "append": "be terse" } } })
        );
        assert_eq!(session_meta(AgentKind::Codex, "be terse"), json!({}));
        // Thinking display rides every Claude Code session, instructions or not.
        assert_eq!(
            session_meta(AgentKind::ClaudeCode, "  ")["claudeCode"]["options"]["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
    }

    /// OpenCode set to use Patchbay's providers: they join its config next to the approval rule.
    /// `PB_PRINT_LAUNCH=1` prints the env, for trying it against a real `opencode acp`.
    #[test]
    fn opencode_launch_carries_patchbay_providers_when_chosen() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let url = std::env::var("PB_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8799/v1".into());
        let p = crate::db::Provider { id: "fw".into(), name: "Fireworks".into(), kind: crate::db::ProviderKind::OpenaiChat, base_url: url, has_key: true, auth_mode: Default::default(), };
        db.upsert_provider(&p, Some("secret")).unwrap();
        let m = crate::db::Model {
            id: "echo".into(),
            provider_id: "fw".into(),
            model_id: "mock-echo".into(),
            display_name: "Echo".into(),
            vision: false,
            context_window: None,
            params: Default::default(),
        };
        db.upsert_model(&m).unwrap();
        let mut o = AgentPaths::new();
        o.insert("opencode".into(), std::env::current_exe().unwrap().to_string_lossy().into_owned());
        let mut prefs = crate::prefs::Preferences { agent_paths: o, ..Default::default() };
        crate::prefs::save(&db, &prefs).unwrap();
        let session = db.create_code_session(&dir.to_string_lossy(), None, "opencode", "default").unwrap();
        let config = |l: &Launch| -> Value {
            serde_json::from_str(&l.env.iter().find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").unwrap().1).unwrap()
        };

        // Off by default: only the approval rule.
        let own = prepare(&db, &dir, &session).unwrap();
        assert_eq!(config(&own), json!({ "permission": { "*": "ask" } }));

        prefs.agent_providers.insert("opencode".into(), "echo".into());
        crate::prefs::save(&db, &prefs).unwrap();
        let l = prepare(&db, &dir, &session).unwrap();
        let c = config(&l);
        assert_eq!(c["permission"], json!({ "*": "ask" }));
        assert_eq!(c["model"], "patchbay-fw/mock-echo");
        assert_eq!(c["provider"]["patchbay-fw"]["options"]["apiKey"], "{env:PATCHBAY_KEY_0}");
        assert!(l.env.iter().any(|(k, v)| k == "PATCHBAY_KEY_0" && v == "secret"));
        if std::env::var_os("PB_PRINT_LAUNCH").is_some() {
            for (k, v) in l.env.iter().filter(|(k, _)| k != "PATH") {
                println!("{k}={v}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex starts in the mode that asks, and gets custom instructions through `CODEX_CONFIG`.
    #[test]
    fn codex_launch_asks_and_carries_instructions() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mut o = AgentPaths::new();
        o.insert("codex".into(), std::env::current_exe().unwrap().to_string_lossy().into_owned());
        let mut prefs = crate::prefs::Preferences { agent_paths: o, ..Default::default() };
        crate::prefs::save(&db, &prefs).unwrap();
        let session = db.create_code_session(&dir.to_string_lossy(), None, "codex", "default").unwrap();
        let env = |l: &Launch, k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        let l = prepare(&db, &dir, &session).unwrap();
        assert_eq!(env(&l, "INITIAL_AGENT_MODE").as_deref(), Some("read-only"));
        assert_eq!(env(&l, "CODEX_CONFIG"), None);
        assert_eq!(l.approvals.select_mode.as_deref(), Some("read-only"));

        prefs.agent_instructions = "be terse".into();
        crate::prefs::save(&db, &prefs).unwrap();
        let l = prepare(&db, &dir, &session).unwrap();
        let config: Value = serde_json::from_str(&env(&l, "CODEX_CONFIG").unwrap()).unwrap();
        assert_eq!(config, json!({ "developer_instructions": "be terse" }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acp_arguments_per_agent() {
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let args = |kind: AgentKind| {
            let mut o = AgentPaths::new();
            o.insert(kind.id().into(), exe.clone());
            resolve(kind, &o).unwrap().args
        };
        assert_eq!(args(AgentKind::Codex), Vec::<String>::new());
        assert_eq!(args(AgentKind::Gemini), vec!["--acp".to_owned()]);
        let agy = args(AgentKind::Antigravity);
        assert_eq!(agy, if cfg!(target_os = "linux") { vec!["--uid=".to_owned()] } else { vec![] });
        // Through npx, the ACP arguments follow the package.
        assert_eq!(AgentKind::Gemini.npx_package(), Some((GEMINI_CLI_PACKAGE, &["--acp"][..])));
        assert_eq!(AgentKind::Antigravity.npx_package(), None);
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
