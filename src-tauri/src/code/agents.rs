//! Coding agent backends for the Code tab.
//!
//! Every backend speaks ACP over stdio, so `acp.rs` drives them all the same way. This module
//! holds what differs between them: how to find and launch the agent, its environment, how
//! Patchbay's custom instructions reach it, and which non-standard extensions it understands.
//! The UI branches on [`Extensions`], never on the agent's identity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use libc;

use anyhow::{Result, anyhow};
use patchbay_agent::proc::search_dirs;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{agent_providers, grok_config};
use crate::db::{CodeSession, Db};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    /// Patchbay's own agent, run inside the app (`crates/patchbay-agent`).
    Patchbay,
    Grok,
    ClaudeCode,
    Opencode,
    Cursor,
    Kilo,
    Codex,
    Antigravity,
}

/// The Claude Code ACP adapter, run through `npx` when it isn't installed.
pub const CLAUDE_ACP_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp@0.93.0";
/// The Codex ACP adapter, which brings its own Codex binary.
pub const CODEX_ACP_PACKAGE: &str = "@agentclientprotocol/codex-acp@2.2.2";

impl AgentKind {
    pub const ALL: [AgentKind; 8] = [
        AgentKind::Patchbay,
        AgentKind::Grok,
        AgentKind::ClaudeCode,
        AgentKind::Opencode,
        AgentKind::Cursor,
        AgentKind::Kilo,
        AgentKind::Codex,
        AgentKind::Antigravity,
    ];

    /// The id stored in `code_sessions.agent` and preferences.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Patchbay => "patchbay",
            AgentKind::Grok => "grok",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Opencode => "opencode",
            AgentKind::Cursor => "cursor",
            AgentKind::Kilo => "kilo",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "antigravity",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Patchbay => "Patchbay",
            AgentKind::Grok => "Grok Build",
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Opencode => "OpenCode",
            AgentKind::Cursor => "Cursor CLI",
            AgentKind::Kilo => "Kilo Code",
            AgentKind::Codex => "Codex",
            AgentKind::Antigravity => "Antigravity",
        }
    }

    /// Executables to look for on PATH, in order, and the arguments that start ACP on stdio.
    fn program(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            AgentKind::Patchbay => (&[], &[]),
            // `prepare_grok` adds the arguments that start its ACP server.
            AgentKind::Grok => (&["grok"], &[]),
            AgentKind::ClaudeCode => (&["claude-agent-acp"], &[]),
            AgentKind::Opencode => (&["opencode"], &["acp"]),
            AgentKind::Cursor => (&["cursor-agent"], &["acp"]),
            AgentKind::Kilo => (&["kilo", "kilocode"], &["acp"]),
            AgentKind::Codex => (&["codex-acp"], &[]),
            // Google ships the ACP server only as an archive (see the ACP registry), so it is
            // rarely on PATH; Settings takes its path. The registry passes `--uid=` on Linux.
            AgentKind::Antigravity => (
                &["agy_acp_server", "agy_acp_server.par", "agy-acp-server"],
                if cfg!(target_os = "linux") {
                    &["--uid="]
                } else {
                    &[]
                },
            ),
        }
    }

    /// The npm package `resolve` falls back to through `npx`, with its ACP arguments.
    fn npx_package(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            AgentKind::ClaudeCode => Some((CLAUDE_ACP_PACKAGE, &[])),
            AgentKind::Codex => Some((CODEX_ACP_PACKAGE, &[])),
            _ => None,
        }
    }

    /// True when the agent runs on Patchbay's providers and models; the others bring their own
    /// sign-in and model list.
    pub fn uses_patchbay_models(self) -> bool {
        matches!(self, AgentKind::Patchbay | AgentKind::Grok)
    }

    /// Runs inside Patchbay: nothing to install or launch.
    pub fn is_builtin(self) -> bool {
        self == AgentKind::Patchbay
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            AgentKind::Patchbay => "Built into Patchbay. Add a provider and model in Settings.",
            AgentKind::Grok => {
                "Install Grok Build (https://github.com/xai-org/grok-build) so that `grok` is on your PATH, or set its path in Settings."
            }
            AgentKind::ClaudeCode => {
                "Install Node.js (Patchbay then runs the adapter through npx), or `npm install -g @agentclientprotocol/claude-agent-acp`. Sign in with `claude`."
            }
            AgentKind::Opencode => {
                "Install from https://opencode.ai, then run `opencode auth login`."
            }
            AgentKind::Cursor => {
                "Install from https://cursor.com/cli, then run `cursor-agent login`."
            }
            AgentKind::Kilo => "`npm install -g @kilocode/cli`, then run `kilo auth login`.",
            AgentKind::Codex => {
                "Install Node.js (Patchbay then runs the adapter through npx), or `npm install -g @agentclientprotocol/codex-acp`. Sign in with `codex login`, or set OPENAI_API_KEY."
            }
            AgentKind::Antigravity => {
                "Google distributes the Antigravity ACP server separately. Click Install in Settings to set it up automatically, or unpack agy_acp_server manually."
            }
        }
    }

    /// How to sign in, for an `auth_required` error.
    pub fn login_hint(self) -> &'static str {
        match self {
            AgentKind::Patchbay | AgentKind::Grok => "Add a provider and model in Settings.",
            AgentKind::ClaudeCode => "Run `claude` in a terminal and sign in with /login.",
            AgentKind::Opencode => "Run `opencode auth login` in a terminal.",
            AgentKind::Cursor => "Run `cursor-agent login` in a terminal.",
            AgentKind::Kilo => "Run `kilo auth login` in a terminal.",
            AgentKind::Codex => {
                "Run `codex login` in a terminal (`npm install -g @openai/codex` gets it), or set OPENAI_API_KEY."
            }
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
    /// Copy the session into a new one, whole or up to a prompt (`patchbay/fork`). Patchbay's own
    /// agent only: ACP's `session/fork` takes no message position (docs/design/session-fork.md).
    pub fork: bool,
    /// Runs on Patchbay's providers and models, so a change to them needs a restart to apply.
    pub patchbay_models: bool,
    /// Grok Build's `x.ai/interject`, which steers a message into the running turn.
    #[serde(skip)]
    pub interject: bool,
    /// A message sent mid-turn joins the running turn, through `interject` or the steering
    /// extension the agent advertises; otherwise Patchbay queues it. Known once the agent starts.
    pub steering: bool,
    /// The host can cancel the running turn to send a message next (Ctrl+Enter, "Send now").
    /// Always set once the agent starts; older hosts leave it out.
    pub force_send: bool,
    /// The agent can stop one running tool call, or one background shell, without ending the
    /// turn (`patchbay/cancel_tool`, `patchbay/kill_shell`). Patchbay's own agent only.
    pub cancel_tool: bool,
    /// A forced send steers the message in instead of cancelling the turn. Claude Code moves the
    /// foreground tool it waits on (a shell command) to the background on a steer, as its own
    /// CLI does; `session/cancel` would kill it. Needs the agent to take steering too.
    #[serde(skip)]
    pub force_steers: bool,
    /// The agent moves its running foreground shell commands to the background on
    /// `patchbay/background_tools`, which a forced send asks for first. Patchbay's own agent only.
    #[serde(skip)]
    pub background_tools: bool,
}

/// What makes an agent ask Patchbay before acting, so the session's approval mode decides
/// (code/permissions.rs, docs/design/approval-modes.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalSetup {
    /// Agent mode to select once the session is up, and again whenever the agent leaves it for
    /// one in `hide_modes`.
    pub select_mode: Option<String>,
    /// Agent modes that are really approval policies of the agent's own; hidden from the UI, and
    /// refused when a caller tries to set one directly (`refuse_mode`).
    pub hide_modes: &'static [&'static str],
    /// What this agent does without asking, whatever the mode. Shown in the session header.
    pub gaps: &'static [&'static str],
    /// `select_mode` is not fixed: it follows the session's approval mode (`codex_mode_for`), at
    /// start, on resume, on a change while running, and in `keep_mode`.
    pub follows_approval: bool,
    /// True when this session is a **current** active dispatched worker (open membership, worker
    /// role, open item, team not archived). Codex Auto then maps to `workspace-write` when the
    /// worktree's common git dir is safe (see [`codex_worker_workspace_write_blocked`]); released /
    /// restored / item-done / standing / non-team sessions stay read-only.
    pub dispatched_worker: bool,
    /// Session cwd used to decide whether a dispatched Codex worker may take `workspace-write`.
    /// Empty for template consent / matrix checks that skip the path gate.
    pub session_cwd: Option<PathBuf>,
}

impl ApprovalSetup {
    /// `for_agent`, with what depends on the session's approval mode filled in (Codex's sandbox).
    pub(crate) fn for_session(
        kind: AgentKind,
        permission_mode: &str,
        dispatched_worker: bool,
        session_cwd: Option<&Path>,
    ) -> Self {
        let mut setup = Self::for_agent(kind);
        if setup.follows_approval {
            setup.dispatched_worker = dispatched_worker;
            setup.session_cwd = session_cwd.map(Path::to_path_buf);
            setup.select_mode =
                Some(codex_mode_for(Some(permission_mode), dispatched_worker, session_cwd).into());
            setup.gaps = codex_gaps_for(Some(permission_mode), dispatched_worker, session_cwd);
        }
        setup
    }

    /// Why a caller (the UI, a mesh peer, a phone) may not set the agent to `mode_id`, or `None`
    /// when it may. The agent's own approval modes are hidden because Patchbay's approval control
    /// decides what runs without asking; setting one directly would remove that control, and
    /// nothing would put it back. Codex's mode follows the approval control, so only the mode that
    /// control implies right now is accepted. `approval_now`: the session's saved approval mode.
    pub(crate) fn refuse_mode(&self, mode_id: &str, approval_now: Option<&str>) -> Option<String> {
        if self.follows_approval {
            let implied =
                codex_mode_for(approval_now, self.dispatched_worker, self.session_cwd.as_deref());
            return (mode_id != implied).then(|| {
                format!("Codex's mode follows the session's approval control ({implied} right now), so \"{mode_id}\" can't be set directly.")
            });
        }
        self.hide_modes.contains(&mode_id).then(|| {
            format!("\"{mode_id}\" is one of the agent's own approval modes, which Patchbay's approval control replaces, so it can't be set directly.")
        })
    }

    pub(crate) fn for_agent(kind: AgentKind) -> Self {
        match kind {
            AgentKind::Patchbay => Self {
                select_mode: None,
                hide_modes: &[],
                gaps: &["Reads and searches inside the project run without asking."],
                ..Self::default()
            },
            // Grok's own mode is pinned to "default" in its config.toml (grok_config.rs).
            AgentKind::Grok | AgentKind::Opencode | AgentKind::Kilo => Self::default(),
            AgentKind::ClaudeCode => Self {
                select_mode: Some("default".into()),
                hide_modes: &["acceptEdits", "auto", "bypassPermissions", "dontAsk"],
                gaps: &[
                    "Rules in your own Claude Code settings (permissions.allow) run without asking.",
                ],
                ..Self::default()
            },
            // Codex's modes are approval and sandbox presets. The session's approval mode picks
            // one (`codex_mode_for`); none is offered in the UI, and the agent is put back in the
            // one the approval mode implies if it leaves it. Starts as the asking one.
            AgentKind::Codex => Self {
                select_mode: Some(CODEX_ASKING_MODE.into()),
                hide_modes: &[CODEX_ASKING_MODE, CODEX_WRITING_MODE, "agent", "agent-full-access"],
                gaps: CODEX_ASKING_GAPS,
                follows_approval: true,
                dispatched_worker: false,
                ..Self::default()
            },
            AgentKind::Antigravity => Self {
                select_mode: Some("default".into()),
                hide_modes: &["auto_edit", "yolo"],
                gaps: &["Antigravity reads and searches files without asking."],
                ..Self::default()
            },
            // Cursor has no per-session approval control over ACP.
            AgentKind::Cursor => Self {
                select_mode: None,
                hide_modes: &[],
                gaps: &[
                    "Cursor edits files in this project without asking, in every mode.",
                    "Shell commands on Cursor's own allowlist (~/.cursor/cli-config.json) run without asking.",
                ],
                ..Self::default()
            },
        }
    }
}

/// Codex mode that asks before edits; also its starting mode (`INITIAL_AGENT_MODE`).
const CODEX_ASKING_MODE: &str = "read-only";
/// Codex mode that edits the project without asking; network and anything outside the sandbox
/// still go through Codex's own `on-request` approvals (pb-20261110-15).
///
/// Under `codex-acp` 2.2.2 the live `workspace-write` turn policy also keeps temp folders writable
/// (`/tmp` and `$TMPDIR` on unix; system `%TEMP%`/`%TMP%` on Windows — `excludeSlashTmp` /
/// `excludeTmpdirEnvVar` hardcoded false). Patchbay cannot confirm those excludes at ACP launch
/// (F1), so a dispatched worker only takes this mode when
/// [`codex_worker_workspace_write_blocked`] is clear — otherwise it stays read-only (F2).
const CODEX_WRITING_MODE: &str = "workspace-write";

/// Hint shown when F2 keeps a Codex worker read-only because the project/git dir is under a
/// temp root (or otherwise under a path workspace-write would make writable).
pub(crate) const CODEX_WORKER_TEMP_READONLY_HINT: &str =
    "Codex worker stays read-only: project is under a temp directory";

/// Hint when F2 refuses workspace-write on Windows: codex-acp sandboxing there is unproven.
pub(crate) const CODEX_WORKER_WINDOWS_READONLY_HINT: &str =
    "Codex worker stays read-only on Windows until codex-acp sandboxing there is proven";

/// `codex-acp` 2.2.2's `workspace-write` / `agent` modes hardcode `excludeSlashTmp` and
/// `excludeTmpdirEnvVar` to false on every turn, so `CODEX_CONFIG` / `[sandbox_workspace_write]`
/// excludes do not stick. Stay false until an adapter (or Patchbay) can confirm them in effect.
pub(crate) const CODEX_WORKER_TEMP_EXCLUDES_CONFIRMED: bool = false;

/// The Codex mode a session's approval mode implies.
///
/// Approve all => `workspace-write`. A **dispatched worker** in Auto => `workspace-write` only when
/// `cwd` is set and [`codex_worker_workspace_write_blocked`] is `None`; omitted `cwd` (template
/// consent / matrix) stays `read-only` so mode mapping never grants writing without a probed path.
/// Everything else => `read-only`. Never `agent` or `agent-full-access`.
pub(crate) fn codex_mode_for(
    permission_mode: Option<&str>,
    dispatched_worker: bool,
    cwd: Option<&Path>,
) -> &'static str {
    match permission_mode {
        Some("always-approve") => CODEX_WRITING_MODE,
        Some("auto") if dispatched_worker => match cwd {
            // Omitted cwd must not grant writing (consent text may still describe the allowed case).
            None => CODEX_ASKING_MODE,
            Some(path) if codex_worker_workspace_write_blocked(path).is_some() => CODEX_ASKING_MODE,
            Some(_) => CODEX_WRITING_MODE,
        },
        _ => CODEX_ASKING_MODE,
    }
}

const CODEX_CONFIG_GAP: &str = "Rules in your own Codex config (~/.codex) may let more run without asking.";
const CODEX_ASKING_GAPS: &[&str] = &[
    "Codex runs shell commands inside its read-only sandbox without asking. It asks before editing files and before network use.",
    CODEX_CONFIG_GAP,
];
const CODEX_WRITING_GAPS: &[&str] = &[
    // Wording is part of worker-auto consent ack; keep "/tmp and $TMPDIR" stable. Windows also
    // treats %TEMP%/%TMP% as temp roots in the F2 gate (see temp_writable_roots).
    "Codex writes in this project and in the temp folders (/tmp and $TMPDIR) without asking. Everything else it asks for (network, files outside the project) is approved automatically under Approve all.",
    CODEX_CONFIG_GAP,
];
/// Dispatched worker in Auto when workspace-write is allowed. Temp roots stay writable under
/// current codex-acp (F1 excludes not confirmed); F2 refuses the mapping when the project's
/// common git dir would fall under those roots or under the worktree itself.
const CODEX_WORKER_AUTO_GAPS: &[&str] = &[
    "As a team worker in Auto, Codex edits files and runs commands inside its worktree sandbox without asking, and also writes under /tmp and $TMPDIR without asking. Network stays off; anything else outside the sandbox still asks. If the team project or its .git is under /tmp or $TMPDIR (or git metadata would be writable inside the worktree), the worker stays read-only instead.",
    CODEX_CONFIG_GAP,
];
const CODEX_WORKER_TEMP_REFUSED_GAPS: &[&str] = &[
    CODEX_WORKER_TEMP_READONLY_HINT,
    "Codex runs shell commands inside its read-only sandbox without asking. It asks before editing files and before network use.",
    CODEX_CONFIG_GAP,
];
const CODEX_WORKER_WINDOWS_REFUSED_GAPS: &[&str] = &[
    CODEX_WORKER_WINDOWS_READONLY_HINT,
    "Codex runs shell commands inside its read-only sandbox without asking. It asks before editing files and before network use.",
    CODEX_CONFIG_GAP,
];

/// What Codex does without asking under a session's approval mode: the header's text.
///
/// `cwd`: when set, a dispatched worker whose project fails the F2 git-dir / Windows gate gets the
/// matching refused gaps. When `None` (template editor / seat consent), worker Auto still uses the
/// workspace-write gaps text so acknowledgement covers the allowed case — but [`codex_mode_for`]
/// with omitted cwd stays read-only.
pub(crate) fn codex_gaps_for(
    permission_mode: Option<&str>,
    dispatched_worker: bool,
    cwd: Option<&Path>,
) -> &'static [&'static str] {
    match permission_mode {
        Some("always-approve") => CODEX_WRITING_GAPS,
        Some("auto") if dispatched_worker => match cwd.map(codex_worker_workspace_write_blocked).flatten() {
            Some(hint) if hint == CODEX_WORKER_WINDOWS_READONLY_HINT => CODEX_WORKER_WINDOWS_REFUSED_GAPS,
            Some(_) => CODEX_WORKER_TEMP_REFUSED_GAPS,
            None => CODEX_WORKER_AUTO_GAPS,
        },
        _ => CODEX_ASKING_GAPS,
    }
}

/// Why a dispatched Codex worker must stay read-only instead of `workspace-write`, or `None`
/// when the mapping is allowed.
///
/// On Windows always refuses ([`CODEX_WORKER_WINDOWS_READONLY_HINT`]) until codex-acp sandboxing
/// there is proven. Elsewhere refuses when the worktree's common git dir or **main** checkout lies
/// under a path Codex would make writable under today's workspace-write policy: the session cwd
/// (worktree), and (while F1 temp excludes are not confirmed) unix `/tmp`/`/private/tmp`/`$TMPDIR`,
/// or Windows `%TEMP%`/`%TMP%`/`$TMPDIR`. A linked worktree is allowed when those live outside the
/// worktree (and outside temp). Fail closed if git cannot be probed, any path canonicalize fails,
/// required temp roots cannot be resolved, or `$TMPDIR` is set but not absolute.
/// (`--show-toplevel` is the current worktree, not the main checkout — use [`git_main_worktree`].)
pub(crate) fn codex_worker_workspace_write_blocked(cwd: &Path) -> Option<&'static str> {
    #[cfg(windows)]
    {
        let _ = cwd;
        return Some(CODEX_WORKER_WINDOWS_READONLY_HINT);
    }
    #[cfg(not(windows))]
    {
    let common = match git_rev_parse_path(cwd, &["--git-common-dir"]) {
        Some(p) => p,
        None => return Some(CODEX_WORKER_TEMP_READONLY_HINT),
    };
    // Main checkout (embedded `.git`, `--separate-git-dir`, submodule): `--git-dir` equals
    // `--git-common-dir`. Linked worktrees use `.../worktrees/<name>` so the paths differ.
    // `git worktree list` alone is wrong for separate-git-dir (lists the gitdir as "main").
    let git_dir = match git_rev_parse_path(cwd, &["--git-dir"]) {
        Some(p) => p,
        None => return Some(CODEX_WORKER_TEMP_READONLY_HINT),
    };
    if git_dir == common {
        return Some(CODEX_WORKER_TEMP_READONLY_HINT);
    }
    let main = match git_main_worktree(cwd) {
        Some(p) => p,
        None => return Some(CODEX_WORKER_TEMP_READONLY_HINT),
    };
    let Some(cwd_key) = path_key(cwd) else {
        return Some(CODEX_WORKER_TEMP_READONLY_HINT);
    };
    if path_under(&common, &cwd_key) || path_under(&main, &cwd_key) {
        return Some(CODEX_WORKER_TEMP_READONLY_HINT);
    }
    if !CODEX_WORKER_TEMP_EXCLUDES_CONFIRMED {
        let Some(roots) = temp_writable_roots() else {
            // Relative $TMPDIR or any temp-root canonicalize failure => refuse mapping.
            return Some(CODEX_WORKER_TEMP_READONLY_HINT);
        };
        for root in roots {
            if path_under(&common, &root) || path_under(&main, &root) {
                return Some(CODEX_WORKER_TEMP_READONLY_HINT);
            }
        }
    }
    None
    } // cfg(not(windows))
}

/// Clear inherited git location / config overrides, then ignore global + system config.
fn apply_hardened_git_env(cmd: &mut std::process::Command) {
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE"] {
        cmd.env_remove(key);
    }
    for (k, _) in std::env::vars() {
        if k.starts_with("GIT_CONFIG_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
}

#[cfg(test)]
thread_local! {
    /// When set, F2 git probes use this program instead of `git` (simulate missing binary).
    static F2_GIT_PROGRAM_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn f2_git_program() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = F2_GIT_PROGRAM_OVERRIDE.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    PathBuf::from("git")
}

fn hardened_git_command(cwd: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(f2_git_program());
    apply_hardened_git_env(&mut cmd);
    cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-C"])
        .arg(cwd);
    cmd
}

/// Hardened `git rev-parse` path result for `cwd`, resolved to an absolute normalised path.
fn git_rev_parse_path(cwd: &Path, rev_args: &[&str]) -> Option<PathBuf> {
    let out = hardened_git_command(cwd)
        .arg("rev-parse")
        .args(rev_args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(&raw);
    let abs = if p.is_absolute() { p } else { cwd.join(p) };
    path_key(&abs)
}

/// Absolute path of the repository's primary (main) worktree, from `git worktree list --porcelain`.
/// Not [`git rev-parse --show-toplevel`], which is the *current* worktree and would always sit
/// under a linked worker cwd.
fn git_main_worktree(cwd: &Path) -> Option<PathBuf> {
    let out = hardened_git_command(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let Some(raw) = line.strip_prefix("worktree ") else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let p = PathBuf::from(raw);
        let abs = if p.is_absolute() { p } else { cwd.join(p) };
        return path_key(&abs);
    }
    None
}

/// Canonical absolute path for comparisons. **No raw-string fallback** — canonicalize failure is
/// `None` so the F2 gate can refuse. On macOS map `/tmp` → `/private/tmp` when still present.
/// On Windows, strip an accepted `\\?\` verbatim prefix so keys match ordinary `%TEMP%` / cwd
/// strings; unknown `\\?\` namespaces fail closed (`None`).
pub(crate) fn path_key(path: &Path) -> Option<PathBuf> {
    let canon = path.canonicalize().ok()?;
    Some(normalize_macos_tmp(crate::win_path::strip_windows_verbatim(canon)?))
}

fn normalize_macos_tmp(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        const TMP: &str = "/tmp";
        const PRIVATE_TMP: &str = "/private/tmp";
        if let Ok(s) = path.strip_prefix(TMP) {
            return PathBuf::from(PRIVATE_TMP).join(s);
        }
    }
    path
}

pub(crate) fn path_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Roots Codex still makes writable under workspace-write while F1 excludes are unconfirmed.
///
/// Returns `None` (caller refuses workspace-write) when a required temp root cannot be resolved, or
/// when `$TMPDIR` is set to a non-empty value that is not absolute or cannot canonicalize. A
/// non-empty value is never trimmed away to "unset" (whitespace-only `$TMPDIR` refuses).
///
/// Unix: `/tmp` (required) plus `/private/tmp` on macOS (required there; optional elsewhere) and
/// `$TMPDIR` when set. Windows has no `/tmp` in the Codex policy sense — use `%TEMP%` / `%TMP%`
/// (at least one must resolve) plus `$TMPDIR` when set; otherwise fail closed.
pub(crate) fn temp_writable_roots() -> Option<Vec<PathBuf>> {
    temp_writable_roots_from(std::env::var("TMPDIR").ok().as_deref())
}

fn temp_writable_roots_from(tmpdir: Option<&str>) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    #[cfg(unix)]
    {
        // /tmp must resolve; refuse the mapping if it cannot.
        roots.push(path_key(Path::new("/tmp"))?);
        // /private/tmp is the macOS real path behind /tmp; require it there. Elsewhere it often does
        // not exist — only include when canonicalize succeeds (do not fail closed on Linux).
        if let Some(private) = path_key(Path::new("/private/tmp")) {
            roots.push(private);
        } else {
            #[cfg(target_os = "macos")]
            {
                return None;
            }
        }
    }
    #[cfg(windows)]
    {
        // Windows: system temp dirs are %TEMP% / %TMP%. Skip empty; refuse if a non-empty value is
        // relative or cannot canonicalize; refuse entirely when neither resolves.
        let mut got = false;
        for key in ["TEMP", "TMP"] {
            let Ok(raw) = std::env::var(key) else { continue };
            if raw.is_empty() {
                continue;
            }
            let p = Path::new(&raw);
            if !p.is_absolute() {
                return None;
            }
            roots.push(path_key(p)?);
            got = true;
        }
        if !got {
            return None;
        }
    }
    // Any non-empty $TMPDIR (including whitespace-only) must be usable as-is.
    // Do not trim-to-unset: whitespace or relative values refuse the mapping.
    if let Some(raw) = tmpdir {
        if !raw.is_empty() {
            let p = Path::new(raw);
            // Relative / non-absolute $TMPDIR is unusable for safe comparison — refuse mapping.
            if !p.is_absolute() {
                return None;
            }
            roots.push(path_key(p)?);
        }
    }
    Some(roots)
}

const OPCODE_ALWAYS_APPROVE_MODE: &str = "always-approve";

/// Config that makes OpenCode and Kilo ask over ACP for every tool. Both read inline config
/// from an env var; deny rules in the user's own config still apply when not overridden here.
fn ask_everything() -> Map<String, Value> {
    Map::from_iter([("permission".to_owned(), json!({ "*": "ask" }))])
}

#[cfg(test)]
thread_local! {
    static OPCODE_DENY_PROBE_OVERRIDE: std::cell::RefCell<Option<std::result::Result<Vec<Value>, String>>> =
        const { std::cell::RefCell::new(None) };
    static OPCODE_PROBE_SPAWN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn normalize_permission_action(key: &str) -> &str {
    if key == "bash" { "shell" } else { key }
}

fn extract_deny_rules_from_permission_object(obj: &serde_json::Map<String, Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for (key, val) in obj {
        let action = normalize_permission_action(key);
        match val {
            Value::String(effect) if effect == "deny" => {
                out.push(json!({ "action": action, "resource": "*", "effect": "deny" }));
            }
            Value::Object(patterns) => {
                for (resource, effect) in patterns {
                    if effect.as_str() == Some("deny") {
                        out.push(
                            json!({ "action": action, "resource": resource, "effect": "deny" }),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Extract deny rules from a single config value (opencode-normalized or legacy forms).
/// Collects from `permissions` array, `permission` object, and `agents.*.permissions`
/// / `agents.*.permission` so no deny source shape is silently dropped.
fn extract_deny_permission_rules(config: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(arr) = config.get("permissions").and_then(Value::as_array) {
        for rule in arr {
            if rule.get("effect").and_then(Value::as_str) == Some("deny") {
                out.push(rule.clone());
            }
        }
    }
    if let Some(obj) = config.get("permission").and_then(Value::as_object) {
        out.extend(extract_deny_rules_from_permission_object(obj));
    }
    if let Some(agents) = config.get("agents").and_then(Value::as_object) {
        for (_name, agent) in agents {
            if let Some(arr) = agent.get("permissions").and_then(Value::as_array) {
                for rule in arr {
                    if rule.get("effect").and_then(Value::as_str) == Some("deny") {
                        out.push(rule.clone());
                    }
                }
            }
            if let Some(obj) = agent.get("permission").and_then(Value::as_object) {
                out.extend(extract_deny_rules_from_permission_object(obj));
            }
        }
    }
    out
}

/// Extract deny rules from all sources returned by opencode's resolved config for the
/// session cwd (fresh probe, without injected content). Covers project/global/env config
/// and opencode's normalized formats as reported by opencode.
fn extract_deny_rules_from_opencode_sources(sources: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(arr) = sources.as_array() else {
        return out;
    };
    for doc in arr {
        if let Some(info) = doc.get("info") {
            out.extend(extract_deny_permission_rules(info));
        }
    }
    out
}

fn probe_http_get_config(port: u16, auth: &str) -> std::result::Result<String, String> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("probe addr parse failed: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("probe connect failed: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!(
        "GET /api/config HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("probe request failed: {e}"))?;
    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .map_err(|e| format!("probe read failed: {e}"))?;
    let resp = String::from_utf8_lossy(&resp);
    let mut parts = resp.splitn(2, "\r\n\r\n");
    let status = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    if !status.contains(" 200 ") {
        return Err(format!(
            "probe config HTTP status not 200 (status_len={})",
            status.len()
        ));
    }
    Ok(body.to_owned())
}

/// Probe opencode's resolved config sources for `cwd` (no injected content) and return deny
/// rules from all sources. Uses a fresh `opencode serve --port 0` child in the session cwd
/// with OPENCODE_CONFIG_CONTENT unset, so config resolves under the session's env/cwd (not
/// the shared background service). Drains stdout/stderr concurrently. Fail-safe: any error
/// => Err, no allows. Error messages MUST NOT include config bodies or credentials.
fn probe_opencode_deny_rules(
    program: &Path,
    cwd: &Path,
) -> std::result::Result<Vec<Value>, String> {
    #[cfg(test)]
    {
        let overridden = OPCODE_DENY_PROBE_OVERRIDE.with(|c| c.borrow().clone());
        if let Some(r) = overridden {
            return r;
        }
        OPCODE_PROBE_SPAWN_COUNT.with(|c| c.set(c.get() + 1));
    }
    use std::io::Read;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("probe port reserve failed: {e}"))?;
    let mut cmd = Command::new(program);
    cmd.args([
        "serve",
        "--port",
        &port.to_string(),
        "--hostname",
        "127.0.0.1",
    ])
    .current_dir(cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .env_remove("OPENCODE_CONFIG_CONTENT");
    #[cfg(test)]
    crate::test_support::isolate_std(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("probe start failed in {}: {e}", cwd.display()))?;
    let child_pid = child.id();

    // Drain pipes concurrently (avoids 64KB pipe deadlock/truncation).
    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = child.stdout.take() {
        let buf = out_buf.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 8192];
            loop {
                match pipe.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut g) = buf.lock() {
                            g.push_str(&String::from_utf8_lossy(&b[..n]));
                        }
                    }
                }
            }
        });
    }
    if let Some(mut pipe) = child.stderr.take() {
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 8192];
            loop {
                match pipe.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut g) = buf.lock() {
                            g.push_str(&String::from_utf8_lossy(&b[..n]));
                        }
                    }
                }
            }
        });
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    let auth = loop {
        let combined = {
            let o = out_buf.lock().ok().map(|g| g.clone()).unwrap_or_default();
            let e = err_buf.lock().ok().map(|g| g.clone()).unwrap_or_default();
            format!("{o}\n{e}")
        };
        if let Some(pw) = combined
            .lines()
            .find_map(|l| l.split("server password ").nth(1))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            use base64::Engine;
            break Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{pw}"))
            ));
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(75));
    };

    let result = (|| {
        let auth = auth.ok_or_else(|| {
            let (ol, el) = (
                out_buf.lock().ok().map(|g| g.len()).unwrap_or(0),
                err_buf.lock().ok().map(|g| g.len()).unwrap_or(0),
            );
            format!(
                "probe not ready in {} (stdout_len={ol}, stderr_len={el})",
                cwd.display()
            )
        })?;
        let body = probe_http_get_config(port, &auth)?;
        // Parse; on failure report length/parse error only (NEVER config body).
        let sources: Value = serde_json::from_str(&body).map_err(|e| {
            format!(
                "probe config parse failed (body_len={}, parse_error={e})",
                body.len()
            )
        })?;
        Ok(extract_deny_rules_from_opencode_sources(&sources))
    })();

    // Kill process group (unix, leader set via process_group(0)) AND direct child on every
    // exit path so launchers/grandchildren do not leak and wait() cannot hang.
    let mut kill_note = String::new();
    #[cfg(unix)]
    {
        // Direct syscall (deterministic; no external kill binary / arg-form / PATH ambiguity).
        // Negative pid targets the process group. ESRCH = already gone => success.
        let rc = unsafe { libc::kill(-(child_pid as i32), libc::SIGKILL) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            if errno != libc::ESRCH {
                kill_note = format!("group_kill_failed(pgid={child_pid},errno={errno})");
                tracing::warn!(cwd=%cwd.display(), pgid=child_pid, rc, errno, "opencode probe process-group kill failed");
            }
        }
    }
    let _ = child.kill();
    let wait_deadline = Instant::now() + Duration::from_secs(2);
    let mut direct_reaped = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => { direct_reaped = true; break; }
            Err(_) => break,
            Ok(None) if Instant::now() > wait_deadline => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    if !direct_reaped {
        let note = format!("direct_child_wait_timeout(pid={child_pid})");
        tracing::warn!(cwd=%cwd.display(), pid=child_pid, "opencode probe child not reaped within bound");
        kill_note = if kill_note.is_empty() { note } else { format!("{kill_note};{note}") };
    }
    // Reader threads are detached and never joined; killing the group/child closes pipe
    // write ends so they observe EOF. All blocking waits above are bounded.
    if kill_note.is_empty() {
        result
    } else {
        result.map_err(|e| format!("{e}; cleanup={kill_note}"))
    }
}

#[derive(Debug)]
struct OpencodeConfigBuild {
    config: Map<String, Value>,
    warning: Option<String>,
    allows_injected: bool,
    deny_rules_reemitted: usize,
}

fn build_opencode_config_from_probe(
    permission_mode: &str,
    providers_config: Map<String, Value>,
    probe: std::result::Result<Vec<Value>, String>,
) -> OpencodeConfigBuild {
    let mut base = ask_everything();
    base.extend(providers_config.clone());
    if permission_mode != OPCODE_ALWAYS_APPROVE_MODE {
        return OpencodeConfigBuild {
            config: base,
            warning: None,
            allows_injected: false,
            deny_rules_reemitted: 0,
        };
    }

    match probe {
        Ok(deny_rules) => {
            // User ask rules for shell/external_directory are intentionally not re-emitted:
            // in always-approve mode Patchbay would auto-approve these; allows replace asks.
            // Specific/pattern deny rules (including any deny for shell/external_directory)
            // are re-emitted AFTER allows so they win.
            let mut permissions = vec![
                json!({ "action": "*", "resource": "*", "effect": "ask" }),
                json!({ "action": "shell", "resource": "*", "effect": "allow" }),
                json!({ "action": "external_directory", "resource": "*", "effect": "allow" }),
            ];
            let deny_rules_reemitted = deny_rules.len();
            permissions.extend(deny_rules);
            let mut config = Map::new();
            config.insert("permissions".to_owned(), Value::Array(permissions));
            config.extend(providers_config);
            OpencodeConfigBuild {
                config,
                warning: None,
                allows_injected: true,
                deny_rules_reemitted,
            }
        }
        Err(err) => {
            let warning = format!(
                "PATCHBAY OPENCODE PERMISSION SAFETY: NOT injecting shell/external_directory allows for always-approve session because opencode's resolved config/denies could not be determined ({}). Falling back to ask-everything so denies cannot be silently outranked.",
                err
            );
            OpencodeConfigBuild {
                config: base,
                warning: Some(warning),
                allows_injected: false,
                deny_rules_reemitted: 0,
            }
        }
    }
}

fn opencode_launch_config(
    permission_mode: &str,
    providers_config: Map<String, Value>,
    opencode_program: &Path,
    cwd: &Path,
) -> String {
    let probe = if permission_mode == OPCODE_ALWAYS_APPROVE_MODE {
        probe_opencode_deny_rules(opencode_program, cwd)
    } else {
        Ok(Vec::new())
    };
    let built = build_opencode_config_from_probe(permission_mode, providers_config, probe);
    if let Some(warning) = &built.warning {
        tracing::error!("{}", warning);
        eprintln!("{}", warning);
    }
    Value::Object(built.config).to_string()
}

/// Where an agent's executable was found.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Runs inside Patchbay.
    Builtin,
    /// The path set in Settings.
    Override,
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
    /// Run this agent inside the app instead of spawning `program`.
    pub native: Option<super::acp::NativeAgent>,
}

/// User overrides from Settings: agent id -> executable path.
pub type AgentPaths = BTreeMap<String, String>;

/// PATH for an agent process: the program's own directory first (an npm shim's `node` usually
/// sits beside it), then [`search_dirs`]. Without this, a shim found in `~/.npm-global/bin`
/// fails with "env: node: No such file" when Patchbay was started from a desktop entry.
fn child_path(program: &Path) -> Option<String> {
    patchbay_agent::proc::child_path(program.parent())
}

/// Locate an executable on [`search_dirs`].
pub fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
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
    // Tests: the pinned home, else the scratch one. Callers write under it (`~/.gemini/...`).
    #[cfg(test)]
    return crate::claude_auth::home().ok();
    #[cfg(not(test))]
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

/// Find the agent's executable: the Settings override, then PATH, and for the agents published
/// on npm a fallback to `npx`.
pub fn resolve(kind: AgentKind, overrides: &AgentPaths) -> Result<Resolved> {
    let (names, acp_args) = kind.program();
    let args: Vec<String> = acp_args.iter().map(|s| (*s).to_owned()).collect();
    if kind.is_builtin() {
        return Ok(Resolved {
            program: PathBuf::new(),
            args,
            source: Source::Builtin,
        });
    }
    if let Some(p) = overrides
        .get(kind.id())
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
    {
        let program = PathBuf::from(p);
        if !program.is_file() {
            return Err(anyhow!("{} path {p} does not exist", kind.label()));
        }
        if kind == AgentKind::Antigravity && is_interactive_cli(&program) {
            return Err(anyhow!(
                "\"{}\" is the Antigravity interactive CLI, not the ACP server. Antigravity requires agy_acp_server.",
                program.display()
            ));
        }
        return Ok(Resolved {
            program,
            args,
            source: Source::Override,
        });
    }
    if let Some(program) = names.iter().find_map(|n| which(n)) {
        return Ok(Resolved {
            program,
            args,
            source: Source::Path,
        });
    }
    if let Some((package, acp_args)) = kind.npx_package()
        && let Some(npx) = which("npx")
    {
        let mut args = vec!["-y".to_owned(), package.to_owned()];
        args.extend(acp_args.iter().map(|s| (*s).to_owned()));
        return Ok(Resolved {
            program: npx,
            args,
            source: Source::Npx,
        });
    }
    Err(anyhow!(
        "{} not found. {}",
        kind.label(),
        kind.install_hint()
    ))
}

/// True if `program` points to the interactive Antigravity CLI (`agy`) rather than its ACP server.
pub fn is_interactive_cli(program: &Path) -> bool {
    let s = program.to_string_lossy();
    let file_part = s.rsplit(['/', '\\']).next().unwrap_or(&s);
    let stem = file_part.strip_suffix(".exe").unwrap_or(file_part);
    stem == "agy"
}

/// The download specification for Google's Antigravity ACP server archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityRelease {
    pub url: &'static str,
    pub cmd: &'static str,
    pub sha256: &'static str,
}

pub fn antigravity_release() -> Option<AntigravityRelease> {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-1.4.3-darwin-arm64.zip",
            cmd: "agy_acp_server.par",
            sha256: "c57db0a9b8b712daa9c3c17cdb990a61842ea569f279e8bd79d7aa85a303b20b",
        })
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-1.4.3-darwin-x86_64.zip",
            cmd: "agy_acp_server.par",
            sha256: "9392dbbdad7f0663dbfb5a0bf641297b0cf80d2ee9cd2d49a616bbbc7378731d",
        })
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.4.3-linux-x86_64.zip",
            cmd: "agy_acp_server.par",
            sha256: "0634ce5306ba92c932945455e7a83fda78e2d99c33fa8aa6ee0566206a382714",
        })
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.4.3-linux-arm64.zip",
            cmd: "agy_acp_server.par",
            sha256: "05c153443b0b1175151c760eeddea538950bc9843e317d54786a1f03670182bc",
        })
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-1.4.3-windows-x86_64.zip",
            cmd: "agy_acp_server.exe",
            sha256: "54c2e16378d9a4952e1da70e355f3c33c4f284308d6de48ab3e6e34a127bd325",
        })
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "aarch64") {
        Some(AntigravityRelease {
            url: "https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-1.4.3-windows-arm64.zip",
            cmd: "agy_acp_server.exe",
            sha256: "fcd46431ad33d3b510ce0b98fe7b43d095bd8b3f1c363ee2d57faa7ee981c7bb",
        })
    } else {
        None
    }
}

/// Download, verify and unpack the official Antigravity ACP server into Patchbay's data directory.
pub async fn install_antigravity(data_dir: &Path) -> Result<PathBuf> {
    use sha2::Digest;

    let release = antigravity_release().ok_or_else(|| {
        anyhow!("Google does not publish an Antigravity ACP server release for this platform")
    })?;
    let dest_dir = data_dir.join("agents").join("antigravity-acp");
    std::fs::create_dir_all(&dest_dir)?;

    let client = reqwest::Client::builder()
        .user_agent(format!("Patchbay/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(release.url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;

    let checksum = hex::encode(sha2::Sha256::digest(&bytes));
    if checksum != release.sha256 {
        anyhow::bail!(
            "Antigravity download checksum mismatch: expected {}, got {}",
            release.sha256,
            checksum
        );
    }

    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor)?;
    zip.extract(&dest_dir)?;

    let binary_path = dest_dir.join(release.cmd);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(&dest_dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file() {
                let mut perms = std::fs::metadata(&p)?.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&p, perms);
            }
        }
    }

    if !binary_path.is_file() {
        anyhow::bail!(
            "expected binary {} was not found in extracted archive",
            binary_path.display()
        );
    }
    Ok(binary_path)
}

fn antigravity_acp_settings() -> Option<PathBuf> {
    let base = std::env::var_os("GEMINI_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".gemini")))?;
    Some(base.join("antigravity-acp").join("settings.json"))
}

/// The current authentication status of Google's Antigravity ACP server, if configured.
pub fn antigravity_auth_status() -> Option<String> {
    let path = antigravity_acp_settings()?;
    if !path.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let auth_type = val.get("auth")?.get("type")?.as_str()?;
    match auth_type {
        "oauth-personal" => Some("Google Account (Personal)".to_owned()),
        "oauth-business" => Some("Google Account (Workspace)".to_owned()),
        "gemini-api-key" => Some("Gemini API key".to_owned()),
        "agent-platform" => Some("Agent Platform".to_owned()),
        other => Some(format!("Signed in ({other})")),
    }
}

/// Remove authentication from Antigravity's settings.json so the next session prompts for sign-in again.
pub fn clear_antigravity_auth() -> Result<()> {
    if let Some(path) = antigravity_acp_settings() {
        if path.is_file() {
            if let Ok(mut val) = std::fs::read_to_string(&path)
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
            {
                if let Some(obj) = val.as_object_mut() {
                    obj.remove("auth");
                }
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            }
        }
    }
    Ok(())
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
    /// Whether Patchbay can download and set up this agent automatically.
    pub can_install: bool,
    /// Human-readable sign-in status (e.g. "Google Account (Personal)"), if known.
    pub auth_status: Option<String>,
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
        can_install: kind == AgentKind::Antigravity && antigravity_release().is_some(),
        auth_status: match kind {
            AgentKind::Antigravity => antigravity_auth_status(),
            _ => None,
        },
    };
    if kind.is_builtin() {
        info.available = true;
        info.source = Some(Source::Builtin);
        info.version = Some(env!("CARGO_PKG_VERSION").to_owned());
        return info;
    }
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
    use patchbay_agent::proc::NoWindow;
    let mut cmd = std::process::Command::new(program);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .no_window();
    #[cfg(test)]
    crate::test_support::isolate_std(&mut cmd);
    let mut child = cmd.spawn().ok()?;
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
                meta.insert(
                    "antigravity".into(),
                    json!({ "systemPrompt": { "append": rules } }),
                );
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
    matches!(kind, AgentKind::Patchbay | AgentKind::Grok | AgentKind::ClaudeCode | AgentKind::Codex | AgentKind::Antigravity)
}

/// Build the launch for a session's agent: executable, arguments, environment and config.
pub fn prepare(db: &std::sync::Arc<Db>, data_dir: &Path, session: &CodeSession) -> Result<Launch> {
    let kind = AgentKind::parse(&session.agent)
        .ok_or_else(|| anyhow!("unknown coding agent {:?}", session.agent))?;
    let prefs = crate::prefs::load(db);
    if kind == AgentKind::Patchbay {
        return prepare_patchbay(db, data_dir, session);
    }
    let resolved = resolve(kind, &prefs.agent_paths)?;
    // Team role instructions ride with the custom instructions for agents that have a prompt hook;
    // the others get them as a preamble on every prompt (`teams::preamble_for`).
    let mut rules = prefs.agent_instructions.trim().to_owned();
    if crate::teams::has_prompt_hook(kind)
        && let Some(team) = crate::teams::instructions_for(data_dir, db, &session.id)
    {
        rules = if rules.is_empty() { team } else { format!("{rules}\n\n{team}") };
    }
    let session_meta = session_meta(kind, &rules);
    if kind == AgentKind::Grok {
        return prepare_grok(db, data_dir, session, resolved, session_meta);
    }
    let mut env = Vec::new();
    if let Some(path) = child_path(&resolved.program) {
        env.push(("PATH".to_owned(), path));
    }
    // Patchbay's providers, only for an agent the user set to use them.
    let providers = match prefs
        .agent_providers
        .get(kind.id())
        .filter(|_| agent_providers::supported(kind))
    {
        Some(model_ref) => agent_providers::setup(db, kind, model_ref)?,
        None => agent_providers::Setup::default(),
    };
    let mut base_config = ask_everything();
    base_config.extend(providers.config.clone());
    let base_config = Value::Object(base_config).to_string();
    match kind {
        AgentKind::Opencode => {
            env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()));
            let config = opencode_launch_config(
                &session.permission_mode,
                providers.config,
                &resolved.program,
                Path::new(&session.cwd),
            );
            env.push(("OPENCODE_CONFIG_CONTENT".to_owned(), config));
        }
        AgentKind::Kilo => env.push(("KILO_CONFIG_CONTENT".to_owned(), base_config)),
        AgentKind::Codex => {
            let dispatched = crate::teams::is_active_dispatched_worker_in(data_dir, &session.id);
            env.push((
                "INITIAL_AGENT_MODE".to_owned(),
                codex_mode_for(Some(&session.permission_mode), dispatched, Some(Path::new(&session.cwd)))
                    .to_owned(),
            ));
            // The adapter reads an API key from the environment only when told to sign in with
            // one; without a login it otherwise stops at `auth_required`.
            if ["CODEX_API_KEY", "OPENAI_API_KEY"]
                .iter()
                .any(|v| std::env::var(v).is_ok_and(|k| !k.trim().is_empty()))
            {
                env.push((
                    "DEFAULT_AUTH_REQUEST".to_owned(),
                    json!({ "methodId": "api-key" }).to_string(),
                ));
            }
            // Merged into every thread's config; `developer_instructions` joins Codex's prompt.
            if !rules.is_empty() {
                env.push((
                    "CODEX_CONFIG".to_owned(),
                    json!({ "developer_instructions": rules }).to_string(),
                ));
            }
        }
        AgentKind::Cursor => {
            env.extend(cursor_preload(data_dir, std::env::var("NODE_OPTIONS").ok()))
        }
        AgentKind::Antigravity => {
            // Google ships `localharness_external` beside `agy_acp_server.par`. When the server is
            // invoked through a symlink or PATH, point ANTIGRAVITY_HARNESS_PATH at it if found beside
            // the canonical binary.
            let prog = resolved
                .program
                .canonicalize()
                .unwrap_or_else(|_| resolved.program.clone());
            if let Some(dir) = prog.parent() {
                for name in ["localharness_external", "localharness"] {
                    let harness = dir.join(name);
                    if harness.is_file() {
                        env.push((
                            "ANTIGRAVITY_HARNESS_PATH".to_owned(),
                            harness.to_string_lossy().into_owned(),
                        ));
                        break;
                    }
                }
            }
        }
        _ => {}
    }
    env.extend(providers.env);
    // A session imported from a Claude account runs with that account's folder, where its files
    // are. Last, so no provider setting overrides it.
    if kind == AgentKind::ClaudeCode
        && let Some(dir) = db.get_setting(&super::import_claude::config_dir_key(&session.id))?
    {
        env.push(("CLAUDE_CONFIG_DIR".to_owned(), dir));
    }
    Ok(Launch {
        program: resolved.program,
        args: resolved.args,
        env,
        env_remove: providers.env_remove,
        ext: Extensions {
            force_steers: kind == AgentKind::ClaudeCode,
            ..Extensions::default()
        },
        session_meta,
        initial_model: session.agent_model.clone(),
        login_hint: kind.login_hint(),
        approvals: ApprovalSetup::for_session(
            kind,
            &session.permission_mode,
            crate::teams::is_active_dispatched_worker_in(data_dir, &session.id),
            Some(Path::new(&session.cwd)),
        ),
        native: None,
    })
}

/// Preloaded into `cursor-agent acp` so its AwaitShell waits reach the client as tool calls.
/// Cursor's ACP adapter drops them, which made a turn waiting on a shell look stuck. The file's
/// header explains how it hooks in without depending on Cursor's exact source.
const CURSOR_PRELOAD: &str = include_str!("cursor_acp_preload.cjs");

/// Environment that loads [`CURSOR_PRELOAD`] into Cursor's Node process, written under
/// `data_dir`. `user_opts` (Patchbay's own NODE_OPTIONS) stays in force, and the preload puts it
/// back for the commands the agent runs. Empty, so Cursor runs unchanged, if the file can't be
/// written.
fn cursor_preload(data_dir: &Path, user_opts: Option<String>) -> Vec<(String, String)> {
    let dir = data_dir.join("cursor");
    let path = dir.join("acp-preload.cjs");
    let current = std::fs::read_to_string(&path).is_ok_and(|s| s == CURSOR_PRELOAD);
    // Through a temporary file, so a Cursor session starting meanwhile never reads half of it.
    let written = current
        || std::fs::create_dir_all(&dir)
            .and_then(|_| {
                let tmp = dir.join(format!("acp-preload.{}.tmp", crate::db::new_id()));
                std::fs::write(&tmp, CURSOR_PRELOAD)?;
                std::fs::rename(&tmp, &path).inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })
            })
            .is_ok();
    if !written {
        tracing::warn!("couldn't write {}; Cursor's AwaitShell calls won't show", path.display());
        return Vec::new();
    }
    let user_opts = user_opts.unwrap_or_default();
    // NODE_OPTIONS takes a double-quoted value with backslash escapes (a path with spaces).
    let quoted = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let opts = format!("--require \"{quoted}\" {user_opts}").trim_end().to_owned();
    vec![
        ("NODE_OPTIONS".to_owned(), opts),
        ("PATCHBAY_ORIG_NODE_OPTIONS".to_owned(), user_opts),
    ]
}

/// Patchbay's own agent: no process, and its models arrive through the host, not a config file.
fn prepare_patchbay(db: &std::sync::Arc<Db>, data_dir: &Path, session: &CodeSession) -> Result<Launch> {
    if db.list_models()?.is_empty() {
        return Err(anyhow!("add a provider and model in Settings first"));
    }
    let initial_model = session
        .model_ref
        .as_deref()
        .filter(|r| db.get_model(r).ok().flatten().is_some())
        .map(grok_config::model_key);
    Ok(Launch {
        program: PathBuf::new(),
        args: Vec::new(),
        env: Vec::new(),
        env_remove: Vec::new(),
        ext: Extensions {
            patchbay_models: true,
            rewind: true,
            fork: true,
            cancel_tool: true,
            force_steers: true,
            background_tools: true,
            ..Extensions::default()
        },
        session_meta: json!({ "model": initial_model }),
        initial_model,
        login_hint: AgentKind::Patchbay.login_hint(),
        approvals: ApprovalSetup::for_agent(AgentKind::Patchbay),
        native: Some(super::native::launcher(db.clone(), data_dir)),
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
        ext: Extensions { status_line: true, rewind: true, fork: false, patchbay_models: true, interject: true, steering: false, force_send: false, cancel_tool: false, force_steers: false, background_tools: false },
        session_meta,
        initial_model: None,
        login_hint: "",
        approvals: ApprovalSetup::for_agent(AgentKind::Grok),
        native: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pb-20261107-32 item 1: restored from staging e8f6b6c (deleted by this PR's earlier
    /// refactor, not rebase noise; `git log e8f6b6c..origin/staging -- agents.rs` is empty).
    #[test]
    fn ids_round_trip() {
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::parse(k.id()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.id()));
        }
        assert_eq!(AgentKind::parse("nope"), None);
    }

    /// pb-20261107-32 item 1: restored from staging e8f6b6c.
    #[test]
    fn instructions_reach_agents_that_read_them() {
        assert_eq!(
            session_meta(AgentKind::Grok, " be terse "),
            json!({ "rules": "be terse" })
        );
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

    #[cfg(test)]
    fn with_probe_override<T>(
        r: std::result::Result<Vec<Value>, String>,
        f: impl FnOnce() -> T,
    ) -> T {
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = Some(r));
        let out = f();
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        out
    }

    fn with_probe_count<T>(f: impl FnOnce() -> T) -> (T, usize) {
        OPCODE_PROBE_SPAWN_COUNT.with(|c| c.set(0));
        let out = f();
        let n = OPCODE_PROBE_SPAWN_COUNT.with(|c| c.get());
        OPCODE_PROBE_SPAWN_COUNT.with(|c| c.set(0));
        (out, n)
    }

    /// pb-20261107-32 E: prepare() output includes allows+re-emitted denies (last) in always-approve.
    #[test]
    fn opencode_prepare_output_always_approve_includes_denies_last() {
        with_probe_override(
            Ok(vec![
                json!({"action":"shell","resource":"rm -rf /tmp/pb*","effect":"deny"}),
            ]),
            || {
                let dir = std::env::temp_dir().join(format!("pb-opc-p-{}", crate::db::new_id()));
                std::fs::create_dir_all(&dir).unwrap();
                let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
                let mut o = AgentPaths::new();
                o.insert(
                    "opencode".into(),
                    std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                let prefs = crate::prefs::Preferences {
                    agent_paths: o,
                    ..Default::default()
                };
                crate::prefs::save(&db, &prefs).unwrap();
                let session = db
                    .create_code_session(
                        &dir.to_string_lossy(),
                        None,
                        "opencode",
                        OPCODE_ALWAYS_APPROVE_MODE,
                    )
                    .unwrap();
                let launch = prepare(&db, &dir, &session).unwrap();
                let cfg: Value = serde_json::from_str(
                    &launch
                        .env
                        .iter()
                        .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                        .unwrap()
                        .1,
                )
                .unwrap();
                let perms = cfg.get("permissions").unwrap().as_array().unwrap();
                assert_eq!(
                    perms[1],
                    json!({"action":"shell","resource":"*","effect":"allow"})
                );
                assert_eq!(
                    perms[2],
                    json!({"action":"external_directory","resource":"*","effect":"allow"})
                );
                let deny = perms.last().unwrap();
                assert_eq!(deny["effect"], "deny");
                assert_eq!(deny["resource"], "rm -rf /tmp/pb*");
                let allow_i = perms
                    .iter()
                    .position(|r| {
                        r["effect"] == "allow" && r["action"] == "shell" && r["resource"] == "*"
                    })
                    .unwrap();
                let deny_i = perms.iter().rposition(|r| r["effect"] == "deny").unwrap();
                assert!(
                    deny_i > allow_i,
                    "deny must be last/after allows in prepare() output"
                );
                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    /// pb-20261107-32 E/B: missing/bad probe => NO allows in prepare() output.
    #[test]
    fn opencode_prepare_output_no_allows_when_probe_fails() {
        with_probe_override(Err("config unreadable".into()), || {
            let dir = std::env::temp_dir().join(format!("pb-opc-pf-{}", crate::db::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
            let mut o = AgentPaths::new();
            o.insert(
                "opencode".into(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            let prefs = crate::prefs::Preferences {
                agent_paths: o,
                ..Default::default()
            };
            crate::prefs::save(&db, &prefs).unwrap();
            let session = db
                .create_code_session(
                    &dir.to_string_lossy(),
                    None,
                    "opencode",
                    OPCODE_ALWAYS_APPROVE_MODE,
                )
                .unwrap();
            let launch = prepare(&db, &dir, &session).unwrap();
            let cfg: Value = serde_json::from_str(
                &launch
                    .env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .1,
            )
            .unwrap();
            assert_eq!(cfg.get("permission"), Some(&json!({"*":"ask"})));
            assert!(
                cfg.get("permissions").is_none(),
                "must not inject allow-bearing permissions when probe fails"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 spec(2)/E: prepare() output, real mode id `default` keeps current ask-everything.
    #[test]
    fn opencode_prepare_output_default_mode_no_allows() {
        let deny = vec![json!({"action":"shell","resource":"git push*","effect":"deny"})];
        with_probe_override(Ok(deny), || {
            let dir = std::env::temp_dir().join(format!("pb-opc-def-{}", crate::db::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
            let mut o = AgentPaths::new();
            o.insert(
                "opencode".into(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            crate::prefs::save(
                &db,
                &crate::prefs::Preferences {
                    agent_paths: o,
                    ..Default::default()
                },
            )
            .unwrap();
            let session = db
                .create_code_session(&dir.to_string_lossy(), None, "opencode", "default")
                .unwrap();
            let launch = prepare(&db, &dir, &session).unwrap();
            let cfg: Value = serde_json::from_str(
                &launch
                    .env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .1,
            )
            .unwrap();
            assert_eq!(
                cfg.get("permission"),
                Some(&json!({"*":"ask"})),
                "default must keep current ask-everything"
            );
            assert!(
                cfg.get("permissions").is_none(),
                "default must not inject permissions array/allows"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 spec(2)/E: prepare() output, real mode id `auto` keeps current ask-everything.
    #[test]
    fn opencode_prepare_output_auto_mode_no_allows() {
        let deny = vec![json!({"action":"shell","resource":"git push*","effect":"deny"})];
        with_probe_override(Ok(deny), || {
            let dir = std::env::temp_dir().join(format!("pb-opc-auto-{}", crate::db::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
            let mut o = AgentPaths::new();
            o.insert(
                "opencode".into(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            crate::prefs::save(
                &db,
                &crate::prefs::Preferences {
                    agent_paths: o,
                    ..Default::default()
                },
            )
            .unwrap();
            let session = db
                .create_code_session(&dir.to_string_lossy(), None, "opencode", "auto")
                .unwrap();
            let launch = prepare(&db, &dir, &session).unwrap();
            let cfg: Value = serde_json::from_str(
                &launch
                    .env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .1,
            )
            .unwrap();
            assert_eq!(
                cfg.get("permission"),
                Some(&json!({"*":"ask"})),
                "auto must keep current ask-everything"
            );
            assert!(
                cfg.get("permissions").is_none(),
                "auto must not inject permissions array/allows"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 spec(2)/E: prepare() output, real mode id `always-approve` injects allows and denies last.
    #[test]
    fn opencode_prepare_output_always_approve_mode_allows_and_denies_last() {
        let deny = vec![json!({"action":"shell","resource":"git push*","effect":"deny"})];
        with_probe_override(Ok(deny), || {
            let dir = std::env::temp_dir().join(format!("pb-opc-aa-{}", crate::db::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
            let mut o = AgentPaths::new();
            o.insert(
                "opencode".into(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            crate::prefs::save(
                &db,
                &crate::prefs::Preferences {
                    agent_paths: o,
                    ..Default::default()
                },
            )
            .unwrap();
            let session = db
                .create_code_session(
                    &dir.to_string_lossy(),
                    None,
                    "opencode",
                    OPCODE_ALWAYS_APPROVE_MODE,
                )
                .unwrap();
            let launch = prepare(&db, &dir, &session).unwrap();
            let cfg: Value = serde_json::from_str(
                &launch
                    .env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .1,
            )
            .unwrap();
            let perms = cfg["permissions"]
                .as_array()
                .expect("always-approve must use permissions array");
            assert!(
                perms.iter().any(|r| r["action"] == "shell"
                    && r["effect"] == "allow"
                    && r["resource"] == "*")
            );
            assert!(perms.iter().any(|r| r["action"] == "external_directory"
                && r["effect"] == "allow"
                && r["resource"] == "*"));
            let deny_i = perms.iter().rposition(|r| r["effect"] == "deny").unwrap();
            let allow_i = perms
                .iter()
                .position(|r| r["action"] == "shell" && r["effect"] == "allow")
                .unwrap();
            assert!(
                deny_i > allow_i,
                "deny must be after allows in prepare() output"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 item 8: probe not spawned for default/auto (mode gate happens before probe).
    #[test]
    fn opencode_probe_not_spawned_for_default_and_auto_modes() {
        let dir = std::env::temp_dir().join(format!("pb-opc-nsp-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Ensure override is clear so a real spawn attempt would count (but should not happen).
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let (_, n_def) = with_probe_count(|| {
            let _ = build_opencode_config_from_probe("default", Map::new(), Ok(vec![]));
            // Also exercise the launch-config path which gates the probe in non-always-approve.
            let _ = opencode_launch_config(
                "default",
                Map::new(),
                Path::new("/nonexistent/opencode"),
                &dir,
            );
        });
        // Note: opencode_launch_config calls probe only in always-approve; build_* takes result.
        // The gate is in opencode_launch_config. With /nonexistent binary, a real probe would fail,
        // but it must not be attempted for default/auto.
        let (_, n_auto) = with_probe_count(|| {
            let _ = opencode_launch_config(
                "auto",
                Map::new(),
                Path::new("/nonexistent/opencode"),
                &dir,
            );
        });
        assert_eq!(n_def, 0, "default mode must not spawn probe");
        assert_eq!(n_auto, 0, "auto mode must not spawn probe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pb-20261107-32 C: mixed sources extract denies from permissions array AND legacy permission object.
    #[test]
    fn opencode_deny_extraction_mixed_formats_from_sources() {
        let sources = json!([
            {"type":"document","info":{"permissions":[{"action":"shell","resource":"ok","effect":"allow"}],"permission":{"bash":{"rm *":"deny"}}}},
            {"type":"document","info":{"permission":{"edit":"deny"}}}
        ]);
        let denies = extract_deny_rules_from_opencode_sources(&sources);
        assert!(
            denies
                .iter()
                .any(|r| r["resource"] == "rm *" && r["effect"] == "deny"),
            "legacy deny in mixed doc: {denies:?}"
        );
        assert!(
            denies
                .iter()
                .any(|r| r["action"] == "edit" && r["effect"] == "deny"),
            "edit deny: {denies:?}"
        );
        assert!(!denies.iter().any(|r| r["effect"] != "deny"));
    }

    /// pb-20261107-32 item 2: agent-level denies in info.agents.<name>.permissions[] (real shape).
    /// Verified shape: {"agents":{"build":{"permissions":[{"action":"shell","resource":"rm *","effect":"deny"}]}}}
    #[test]
    fn opencode_deny_extraction_agent_level_permissions_real_shape() {
        let sources = json!([
            {"type":"document","path":"/tmp/opencode.json","info":{
                "agents":{"build":{"permissions":[{"action":"shell","resource":"rm *","effect":"deny"}]}}
            }}
        ]);
        let denies = extract_deny_rules_from_opencode_sources(&sources);
        assert_eq!(denies.len(), 1, "must extract agent-level deny: {denies:?}");
        assert_eq!(denies[0]["action"], "shell");
        assert_eq!(denies[0]["resource"], "rm *");
        assert_eq!(denies[0]["effect"], "deny");
    }

    /// pb-20261107-32 item 5: committed fixture using REAL probe-source output shape
    /// (array of {type,path,info:{permissions}} incl. a directory doc), consistent with the
    /// fresh `opencode serve --port 0` + /api/config probe (item 7).
    #[test]
    fn opencode_deny_extraction_real_serve_api_config_source_shape() {
        let sources = json!([
            {"type":"directory","path":"/tmp/project","info":{}},
            {"type":"document","path":"/tmp/home/.config/opencode/opencode.json","info":{
                "$schema":"https://opencode.ai/config.json",
                "permissions":[{"action":"shell","resource":"git push*","effect":"deny"}]
            }},
            {"type":"document","path":"/tmp/project/opencode.json","info":{
                "permissions":[{"action":"shell","resource":"echo PROJ*","effect":"deny"},{"action":"shell","resource":"*","effect":"allow"}],
                "agents":{"build":{"permissions":[{"action":"external_directory","resource":"/secret","effect":"deny"}]}}
            }},
            {"type":"document","path":null,"info":{"permissions":[{"action":"*","resource":"*","effect":"ask"}]}}
        ]);
        let denies = extract_deny_rules_from_opencode_sources(&sources);
        assert!(
            denies.iter().any(|r| r["action"] == "shell"
                && r["resource"] == "git push*"
                && r["effect"] == "deny"),
            "global deny: {denies:?}"
        );
        assert!(
            denies.iter().any(|r| r["action"] == "shell"
                && r["resource"] == "echo PROJ*"
                && r["effect"] == "deny"),
            "project deny: {denies:?}"
        );
        assert!(
            denies.iter().any(|r| r["action"] == "external_directory"
                && r["resource"] == "/secret"
                && r["effect"] == "deny"),
            "agent-level deny: {denies:?}"
        );
        assert!(
            !denies.iter().any(|r| r["effect"] != "deny"),
            "only denies: {denies:?}"
        );
    }



    #[cfg(unix)]
    fn with_test_timeout<T: Send + 'static>(secs: u64, name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || { let _ = tx.send(f()); });
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(v) => v,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{name}: timed out after {secs}s (unbounded hang guard)")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Worker panicked/dropped sender — join to propagate the real panic (do not mask as timeout).
                match handle.join() {
                    Err(payload) => std::panic::resume_unwind(payload),
                    Ok(_) => panic!("{name}: worker disconnected without result (not a timeout)"),
                }
            }
        }
    }
    #[cfg(unix)]
    fn write_fake_opencode(dir: &Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
    /// pb-20261107-32 item 4 (real probe, fake program): parse/HTTP errors must not leak
    /// config body or credentials. Fake serves 200 with bad JSON body containing
    /// sk-FAKE-SECRET-123, and 404 containing the secret — Err must lack the secret.
    #[cfg(unix)]
    #[test]
    fn opencode_probe_errors_contain_no_config_body_or_credentials_real_probe() {
        with_test_timeout(30, "opencode_probe_errors_contain_no_config_body_or_credentials_real_probe", || {
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let dir = std::env::temp_dir().join(format!("pb-opc-sec-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let base = r#"#!/usr/bin/env python3
import sys, socket, threading
port = int(sys.argv[sys.argv.index('--port')+1])
sys.stderr.write('server password testpw\n'); sys.stderr.flush()
MODE = '__MODE__'
def handle(c):
    try:
        c.recv(4096)
        if MODE == 'notfound':
            body = '{"error":"sk-FAKE-SECRET-123","Authorization":"Bearer TOPSECRET"}'
            status = '404 Not Found'
        else:
            body = 'NOT_JSON sk-FAKE-SECRET-123 {"Authorization":"Bearer TOPSECRET"}'
            status = '200 OK'
        c.sendall(('HTTP/1.1 %s\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s' % (status,len(body.encode()),body)).encode())
    except Exception: pass
    finally:
        try: c.close()
        except: pass
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',port)); s.listen(5)
while True:
    c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
"#;
        let p_bad = write_fake_opencode(&dir, "fake-badbody", &base.replace("__MODE__", "badbody"));
        let err = probe_opencode_deny_rules(&p_bad, &cwd).unwrap_err();
        assert!(!err.contains("sk-FAKE-SECRET"), "bad-body Err must not leak secret: {err}");
        assert!(!err.contains("TOPSECRET"), "bad-body Err must not leak credential: {err}");
        assert!(!err.contains("NOT_JSON"), "bad-body Err must not leak body: {err}");
        assert!(err.contains("body_len=") || err.contains("parse"), "bad-body Err should indicate parse/length: {err}");
        let p_nf = write_fake_opencode(&dir, "fake-notfound", &base.replace("__MODE__", "notfound"));
        let err2 = probe_opencode_deny_rules(&p_nf, &cwd).unwrap_err();
        assert!(!err2.contains("sk-FAKE-SECRET"), "404 Err must not leak secret: {err2}");
        assert!(!err2.contains("TOPSECRET"), "404 Err must not leak credential: {err2}");
        let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 item 3 (real probe, fake program): drain handles >64KB output before password.
    /// Fake prints 70KB + "server password testpw" then serves 200/[] on the probe port.
    /// Planted break-after-first-chunk in production drain must go RED (timeout / password not found).
    #[cfg(unix)]
    #[test]
    fn opencode_probe_drains_large_output_real_probe() {
        with_test_timeout(30, "opencode_probe_drains_large_output_real_probe", || {
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let dir = std::env::temp_dir().join(format!("pb-opc-drain-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let script = r#"#!/usr/bin/env python3
import sys, socket, threading
port = int(sys.argv[sys.argv.index('--port')+1])
# >64KB before password so a broken/single-chunk drain misses it
sys.stdout.write('X' * 70000 + '\n')
sys.stdout.write('server password testpw\n')
sys.stdout.flush()
body = '[]'
def handle(c):
    try:
        c.recv(4096)
        c.sendall(('HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s' % (len(body),body)).encode())
    except Exception: pass
    finally:
        try: c.close()
        except: pass
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',port)); s.listen(5)
while True:
    c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
"#;
        let prog = write_fake_opencode(&dir, "fake-drain", script);
        let denies = probe_opencode_deny_rules(&prog, &cwd).expect("probe must drain >64KB and find password");
        assert!(denies.is_empty(), "expected empty denies from [] config: {denies:?}");
        let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 item 7 (real probe, fake program): argv is `serve --port <n> --hostname 127.0.0.1`
    /// with NO --service/debug; resolves under session cwd. Planted --service must go RED.
    #[cfg(unix)]
    #[test]
    fn opencode_probe_argv_fresh_session_serve_no_service_real_probe() {
        with_test_timeout(30, "opencode_probe_argv_fresh_session_serve_no_service_real_probe", || {
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let dir = std::env::temp_dir().join(format!("pb-opc-argv-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let argv_file = dir.join("argv.txt");
        let script = format!(r#"#!/usr/bin/env python3
import sys, socket, threading, json
with open({argv:?}, 'w') as f: json.dump(sys.argv, f)
port = int(sys.argv[sys.argv.index('--port')+1])
sys.stderr.write('server password testpw\n'); sys.stderr.flush()
body = '[]'
def handle(c):
    try:
        c.recv(4096)
        c.sendall(('HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s' % (len(body),body)).encode())
    except Exception: pass
    finally:
        try: c.close()
        except: pass
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',port)); s.listen(5)
while True:
    c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
"#, argv=argv_file.to_string_lossy());
        let prog = write_fake_opencode(&dir, "fake-argv", &script);
        let denies = probe_opencode_deny_rules(&prog, &cwd).expect("probe should succeed");
        assert!(denies.is_empty());
        let argv: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&argv_file).unwrap()).unwrap();
        assert!(argv.iter().any(|a| a == "serve"), "argv must contain 'serve': {argv:?}");
        assert!(argv.iter().any(|a| a == "--port"), "argv must contain --port: {argv:?}");
        assert!(argv.iter().any(|a| a == "--hostname"), "argv must contain --hostname: {argv:?}");
        assert!(argv.iter().any(|a| a == "127.0.0.1"), "argv must contain 127.0.0.1: {argv:?}");
        assert!(!argv.iter().any(|a| a == "--service"), "argv must NOT contain --service (shared service): {argv:?}");
        assert!(!argv.iter().any(|a| a == "debug"), "argv must NOT contain debug (debug config path): {argv:?}");
        let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// pb-20261107-32 item 2 (process leak): fake launcher spawns `sleep 302 &` (grandchild)
    /// and never serves HTTP (conn-refused). Probe must kill the whole process group so the
    /// grandchild is gone on exit (npm-style launchers would otherwise leak one server per start).
    #[cfg(unix)]
    #[test]
    fn opencode_probe_kills_process_group_no_grandchild_leak() {
        with_test_timeout(30, "opencode_probe_kills_process_group_no_grandchild_leak", || {
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let dir = std::env::temp_dir().join(format!("pb-opc-leak-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let pid_file = dir.join("grandchild.pid");
        let script = format!(r#"#!/usr/bin/env python3
import sys, os, time
# Grandchild inherits the launcher's process group (probe set process_group(0) on launcher).
# Do NOT setsid: a plain backgrounded grandchild stays in the group and must be reaped by killpg.
pid = os.fork()
if pid == 0:
    os.execvp('sleep', ['sleep', '302'])
with open({pidf:?}, 'w') as f: f.write(str(pid))
sys.stderr.write('server password testpw\n'); sys.stderr.flush()
# never listen on --port => conn refused; stay alive so group-kill is what reaps us
time.sleep(60)
"#, pidf=pid_file.to_string_lossy());
        let prog = write_fake_opencode(&dir, "fake-leak", &script);
        let err = probe_opencode_deny_rules(&prog, &cwd).unwrap_err();
        assert!(err.contains("probe") || err.contains("connect") || err.contains("read") || err.contains("HTTP") || err.contains("ready"),
            "expected probe failure on conn-refused path, got: {err}");
        // give a moment for signals
        std::thread::sleep(std::time::Duration::from_millis(150));
        let gpid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        // Poll up to ~2s (SIGKILL async). Zombie-aware: kill(pid,0) succeeds for zombies; on Linux
        // /proc/<pid>/stat state 'Z' means not a live leak. Missing/zombie => gone.
        // Use libc (not shell `kill`) so ESRCH does not print "kill: …: No such process".
        let is_live = || {
            let sig0 = unsafe { libc::kill(gpid, 0) == 0 };
            if !sig0 {
                return false;
            }
            match std::fs::read_to_string(format!("/proc/{gpid}/stat")) {
                Ok(stat) => !stat
                    .split(')')
                    .nth(1)
                    .and_then(|r| r.trim_start().chars().next())
                    .map(|c| c == 'Z')
                    .unwrap_or(false),
                Err(_) => sig0, // no /proc (e.g. macOS): fall back to kill(pid, 0)
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
        let mut alive = is_live();
        while alive && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            alive = is_live();
        }
        if alive {
            unsafe {
                let _ = libc::kill(gpid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!alive, "grandchild pid {gpid} leaked (live, not zombie) after probe returned ({err})");
        });
    }

    /// pb-20261107-32 item 5: #[ignore] live test calling the REAL probe (fresh serve child,
    /// session cwd/env). Requires `opencode` on PATH. Exercises the same source the fix uses.
    #[test]
    // Manual test. If it needs your real home or logins, run it with PB_TEST_REAL_HOME=1 (see test_support).
    #[ignore]
    fn opencode_live_probe_reads_session_cwd_config_and_agent_denies() {
        let opencode = std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".into());
        let dir = std::env::temp_dir().join(format!("pb-opc-live-{}", crate::db::new_id()));
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        // Project-level deny + agent-level deny (item 2 real shape)
        std::fs::write(project.join("opencode.json"), r#"{"permissions":[{"action":"shell","resource":"echo LIVE_DENY*","effect":"deny"}],"agents":{"build":{"permissions":[{"action":"shell","resource":"rm LIVE*","effect":"deny"}]}}}"#).unwrap();
        OPCODE_DENY_PROBE_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let denies = probe_opencode_deny_rules(Path::new(&opencode), &project)
            .expect("live probe should succeed");
        assert!(
            denies
                .iter()
                .any(|r| r["resource"] == "echo LIVE_DENY*" && r["effect"] == "deny"),
            "project deny not seen by live probe: {denies:?}"
        );
        assert!(
            denies
                .iter()
                .any(|r| r["resource"] == "rm LIVE*" && r["effect"] == "deny"),
            "agent-level deny not seen by live probe (item 2): {denies:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pb-20261107-32 D ordering pin: deny re-emitted after allows.
    #[test]
    fn opencode_deny_rules_appended_after_allows() {
        let built = build_opencode_config_from_probe(
            OPCODE_ALWAYS_APPROVE_MODE,
            Map::new(),
            Ok(vec![
                json!({"action":"shell","resource":"*","effect":"deny"}),
            ]),
        );
        let perms = built.config.get("permissions").unwrap().as_array().unwrap();
        let a = perms
            .iter()
            .position(|r| r["action"] == "shell" && r["effect"] == "allow" && r["resource"] == "*")
            .unwrap();
        let d = perms
            .iter()
            .position(|r| r["action"] == "shell" && r["effect"] == "deny" && r["resource"] == "*")
            .unwrap();
        assert!(d > a);
    }

    /// pb-20261107-32 B: probe failure / unreadable => no allows + warning.
    #[test]
    fn opencode_probe_failure_means_no_allows() {
        let built = build_opencode_config_from_probe(
            OPCODE_ALWAYS_APPROVE_MODE,
            Map::new(),
            Err("cannot read".into()),
        );
        assert!(!built.allows_injected);
        assert!(
            built
                .warning
                .as_deref()
                .unwrap_or("")
                .contains("NOT injecting")
        );
        assert_eq!(built.config["permission"], json!({"*":"ask"}));
    }

    /// pb-20261107-32 D/B: probe success with zero denies => allows OK (file exists+parses+ZERO denies case).
    #[test]
    fn opencode_probe_success_zero_denies_allows_injected() {
        let built =
            build_opencode_config_from_probe(OPCODE_ALWAYS_APPROVE_MODE, Map::new(), Ok(vec![]));
        assert!(built.allows_injected);
        assert_eq!(built.deny_rules_reemitted, 0);
    }

    /// pb-20261107-32 item 1: restored doc comment context from staging e8f6b6c.
    /// OpenCode set to use Patchbay's providers: they join its config next to the approval rule.
    /// `PB_PRINT_LAUNCH=1` prints the env, for trying it against a real `opencode acp`.
    #[test]
    fn opencode_launch_carries_patchbay_providers_when_chosen() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
        let url =
            std::env::var("PB_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:8799/v1".into());
        let p = crate::db::Provider {
            id: "fw".into(),
            name: "Fireworks".into(),
            kind: crate::db::ProviderKind::OpenaiChat,
            base_url: url,
            has_key: true,
            auth_mode: Default::default(),
            claude_config_dir: None,
            grok_home: None,
        };
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
        o.insert(
            "opencode".into(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        let mut prefs = crate::prefs::Preferences {
            agent_paths: o,
            ..Default::default()
        };
        crate::prefs::save(&db, &prefs).unwrap();
        let session = db
            .create_code_session(&dir.to_string_lossy(), None, "opencode", "default")
            .unwrap();
        let config = |l: &Launch| -> Value {
            serde_json::from_str(
                &l.env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .1,
            )
            .unwrap()
        };

        // Off by default: only the approval rule.
        let own = prepare(&db, &dir, &session).unwrap();
        assert_eq!(config(&own), json!({ "permission": { "*": "ask" } }));

        prefs
            .agent_providers
            .insert("opencode".into(), "echo".into());
        crate::prefs::save(&db, &prefs).unwrap();
        let l = prepare(&db, &dir, &session).unwrap();
        let c = config(&l);
        assert_eq!(c["permission"], json!({ "*": "ask" }));
        assert_eq!(c["model"], "patchbay-fw/mock-echo");
        assert_eq!(
            c["provider"]["patchbay-fw"]["options"]["apiKey"],
            "{env:PATCHBAY_KEY_0}"
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "PATCHBAY_KEY_0" && v == "secret")
        );
        if std::env::var_os("PB_PRINT_LAUNCH").is_some() {
            for (k, v) in l.env.iter().filter(|(k, _)| k != "PATH") {
                println!("{k}={v}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ask first => read-only: Codex starts in the mode that asks, and gets custom instructions
    /// through `CODEX_CONFIG` (pb-20261110-15 keeps this pin and names it for what it holds).
    #[test]
    fn codex_launch_under_ask_is_read_only_and_carries_instructions() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
        let mut o = AgentPaths::new();
        o.insert(
            "codex".into(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        let mut prefs = crate::prefs::Preferences {
            agent_paths: o,
            ..Default::default()
        };
        crate::prefs::save(&db, &prefs).unwrap();
        let session = db
            .create_code_session(&dir.to_string_lossy(), None, "codex", "default")
            .unwrap();
        let env = |l: &Launch, k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        let l = prepare(&db, &dir, &session).unwrap();
        assert_eq!(env(&l, "INITIAL_AGENT_MODE").as_deref(), Some("read-only"));
        assert_eq!(env(&l, "CODEX_CONFIG"), None);
        assert_eq!(l.approvals.select_mode.as_deref(), Some("read-only"));
        assert_eq!(l.approvals.gaps, codex_gaps_for(Some("default"), false, Some(Path::new(&session.cwd))));

        prefs.agent_instructions = "be terse".into();
        crate::prefs::save(&db, &prefs).unwrap();
        let l = prepare(&db, &dir, &session).unwrap();
        let config: Value = serde_json::from_str(&env(&l, "CODEX_CONFIG").unwrap()).unwrap();
        assert_eq!(config, json!({ "developer_instructions": "be terse" }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Codex session is launched from its SAVED approval mode, every time: Approve all => the
    /// write-in-project mode (reaching the process as `INITIAL_AGENT_MODE` and as the mode to
    /// select), anything else => read-only; a resume is just another `prepare` (pb-20261110-15).
    #[test]
    fn codex_launch_follows_the_saved_approval_mode_including_on_resume() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
        let mut o = AgentPaths::new();
        o.insert("codex".into(), std::env::current_exe().unwrap().to_string_lossy().into_owned());
        crate::prefs::save(&db, &crate::prefs::Preferences { agent_paths: o, ..Default::default() }).unwrap();
        let session = db.create_code_session(&dir.to_string_lossy(), None, "codex", "default").unwrap();
        let env = |l: &Launch, k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let launch = || prepare(&db, &dir, &db.get_code_session(&session.id).unwrap().unwrap()).unwrap();

        for (approval, mode) in [
            ("always-approve", "workspace-write"),
            ("default", "read-only"),
            ("auto", "read-only"),
            // Switched back and forth, each (re)start reads what is saved now.
            ("always-approve", "workspace-write"),
            ("default", "read-only"),
        ] {
            db.set_code_session_permission_mode(&session.id, approval).unwrap();
            let l = launch();
            assert_eq!(env(&l, "INITIAL_AGENT_MODE").as_deref(), Some(mode), "{approval}");
            assert_eq!(l.approvals.select_mode.as_deref(), Some(mode), "{approval}");
            assert_eq!(
                l.approvals.gaps,
                codex_gaps_for(Some(approval), false, Some(Path::new(&session.cwd))),
                "{approval}"
            );
            assert!(l.approvals.follows_approval);
            assert!(!l.approvals.dispatched_worker);
            // The write-capable mode is never offered, and neither are the agent modes.
            for hidden in ["read-only", "workspace-write", "agent", "agent-full-access"] {
                assert!(l.approvals.hide_modes.contains(&hidden), "{hidden} stays hidden");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mapping matrix: only Approve all opens write without a probed cwd. A dispatched worker in
    /// Auto with omitted cwd stays read-only (mode never grants writing from consent alone);
    /// gaps text for omitted cwd still describes the allowed workspace-write case. Standing seats,
    /// non-team Auto, and worker Default stay read-only. No input reaches `agent` /
    /// `agent-full-access`.
    #[test]
    fn codex_mode_mapping_matrix() {
        assert_eq!(codex_mode_for(Some("always-approve"), false, None), "workspace-write");
        assert_eq!(codex_mode_for(Some("always-approve"), true, None), "workspace-write");
        assert_eq!(
            codex_mode_for(Some("auto"), true, None),
            "read-only",
            "worker+auto with omitted cwd must not grant writing"
        );
        assert_eq!(codex_mode_for(Some("default"), true, None), "read-only", "worker+default");
        assert_eq!(codex_mode_for(Some("auto"), false, None), "read-only", "standing/non-team auto");
        assert_eq!(codex_mode_for(Some("default"), false, None), "read-only");
        for other in [None, Some(""), Some("agent"), Some("agent-full-access"), Some("workspace-write")] {
            assert_eq!(codex_mode_for(other, false, None), "read-only", "{other:?}");
            assert_eq!(codex_mode_for(other, true, None), "read-only", "{other:?}");
        }
        let worker_auto = codex_gaps_for(Some("auto"), true, None).join(" ");
        assert!(worker_auto.contains("team worker in Auto") && worker_auto.contains("still ask"), "{worker_auto}");
        assert!(worker_auto.contains("/tmp") && worker_auto.contains("$TMPDIR"), "{worker_auto}");
        assert!(!worker_auto.contains("approved automatically"), "{worker_auto}");
        assert_eq!(
            codex_gaps_for(Some("auto"), false, None),
            codex_gaps_for(Some("default"), false, None)
        );
        assert_eq!(
            codex_gaps_for(Some("default"), true, None),
            codex_gaps_for(Some("default"), false, None)
        );
    }

    #[test]
    fn codex_mode_for_omitted_cwd_read_only_even_when_gaps_optimistic() {
        assert_eq!(codex_mode_for(Some("auto"), true, None), "read-only");
        let gaps = codex_gaps_for(Some("auto"), true, None).join(" ");
        assert!(gaps.contains("team worker in Auto"), "{gaps}");
    }

    #[test]
    fn strip_windows_verbatim_accepts_drive_and_unc_refuses_other_namespaces() {
        use crate::win_path::strip_windows_verbatim_str;
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\Users\x\proj"),
            Some(r"C:\Users\x\proj".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\c:\temp"),
            Some(r"c:\temp".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\server\share\path"),
            Some(r"\\server\share\path".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\srv\share"),
            Some(r"\\srv\share".into())
        );
        // Not verbatim — leave alone.
        assert_eq!(strip_windows_verbatim_str(r"C:\x"), Some(r"C:\x".into()));
        // Refuse bare drive, incomplete UNC, and other \\?\ namespaces.
        for bad in [
            r"\\?\C:",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x",
            r"\\?\HarddiskVolume1\Windows",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x",
            r"\\?\foo",
            r"\\?\UNC\serveronly",
            r"\\?\UNC\",
            r"\\?\C",
            r"\\?\C:noslash",
        ] {
            assert_eq!(strip_windows_verbatim_str(bad), None, "must refuse {bad}");
        }
    }

    #[test]
    fn codex_worker_windows_refuse_hint_constant_is_distinct() {
        assert_ne!(CODEX_WORKER_WINDOWS_READONLY_HINT, CODEX_WORKER_TEMP_READONLY_HINT);
        assert!(CODEX_WORKER_WINDOWS_READONLY_HINT.contains("Windows"));
        #[cfg(windows)]
        {
            let hint = codex_worker_workspace_write_blocked(Path::new(r"C:\any"));
            assert_eq!(hint, Some(CODEX_WORKER_WINDOWS_READONLY_HINT));
            assert_eq!(
                codex_mode_for(Some("auto"), true, Some(Path::new(r"C:\any"))),
                "read-only"
            );
            let gaps = codex_gaps_for(Some("auto"), true, Some(Path::new(r"C:\any"))).join(" ");
            assert!(gaps.contains("Windows"), "{gaps}");
        }
    }

    /// The header says what is true in each state: Ask asks for edits and network; Approve all
    /// writes in the project and the temp folders unasked and approves the rest itself.
    #[test]
    fn codex_header_text_differs_per_state() {
        let ask = codex_gaps_for(Some("default"), false, None).join(" ");
        let all = codex_gaps_for(Some("always-approve"), false, None).join(" ");
        assert_ne!(ask, all);
        assert!(ask.contains("read-only sandbox") && ask.contains("asks before editing files"), "{ask}");
        assert!(!ask.contains("temp folders"), "{ask}");
        assert!(all.contains("writes in this project") && all.contains("/tmp and $TMPDIR"), "{all}");
        assert!(all.contains("network, files outside the project") && all.contains("approved automatically"), "{all}");
        // Unreadable approval mode reads as Ask, text included.
        assert_eq!(
            codex_gaps_for(None, false, None),
            codex_gaps_for(Some("default"), false, None)
        );
        // Other agents keep a fixed setup whatever the session's approval mode is.
        let claude = ApprovalSetup::for_session(AgentKind::ClaudeCode, "always-approve", false, None);
        assert_eq!(claude, ApprovalSetup::for_agent(AgentKind::ClaudeCode));
        assert!(!claude.follows_approval);
    }

    /// Seed an active dispatched worker membership + open item for `session`.
    fn seed_active_worker(dir: &std::path::Path, team_id: &str, session: &str, item: &str, status: &str) {
        let conn = rusqlite::Connection::open(dir.join("teams.db")).unwrap();
        conn.execute(
            "INSERT INTO items(team_id, id, title, type, severity, status, plan, created_at, updated_at)
             VALUES (?1, ?2, ?2, 'bug', 'low', ?3, '', 0, 0)",
            rusqlite::params![team_id, item, status],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO members(team_id, session_id, seat, role_id, status, item_id, joined_at)
             VALUES (?1, ?2, 'workers', 'builtin-worker', 'active', ?3, 0)",
            rusqlite::params![team_id, session, item],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatches(team_id, item_id, seat, desk_session, session_id, at)
             VALUES (?1, ?2, 'workers', 'desk', ?3, 0)",
            rusqlite::params![team_id, item, session],
        )
        .unwrap();
    }

    /// Under a temp_dir team (F2), a current active Codex worker in Auto stays read-only; released /
    /// item-done stay read-only too. Workspace-write needs a safe linked worktree (see next test).
    #[test]
    fn codex_dispatched_worker_auto_launches_workspace_write() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
        let mut o = AgentPaths::new();
        o.insert("codex".into(), std::env::current_exe().unwrap().to_string_lossy().into_owned());
        crate::prefs::save(&db, &crate::prefs::Preferences { agent_paths: o, ..Default::default() }).unwrap();
        let session = db
            .create_code_session(&dir.to_string_lossy(), None, "codex", "auto")
            .unwrap();
        let tdb = crate::teams::TeamsDb::open(&dir).unwrap();
        let team = tdb
            .create_team(crate::teams::NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "demo".into(),
                cwd: dir.join("proj").to_string_lossy().into_owned(),
                id_prefix: Some("DM".into()),
                pr_base: None,
                worktree_root: Some(dir.join("proj/trees").to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(dir.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        seed_active_worker(&dir, &team.id, &session.id, "i1", "in-progress");
        assert!(crate::teams::is_active_dispatched_worker_in(&dir, &session.id));
        assert!(crate::teams::is_dispatched_in(&dir, &session.id));
        let l = prepare(&db, &dir, &session).unwrap();
        let mode = |k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        // F2: temp roots stay writable under today's workspace-write, so the worker stays read-only.
        assert_eq!(mode("INITIAL_AGENT_MODE"), Some("read-only"));
        assert_eq!(l.approvals.select_mode.as_deref(), Some("read-only"));
        assert!(l.approvals.dispatched_worker);
        assert_eq!(
            l.approvals.gaps,
            codex_gaps_for(Some("auto"), true, Some(Path::new(&session.cwd)))
        );
        assert!(
            l.approvals.gaps.iter().any(|g| g.contains("temp directory") || g.contains("Windows")),
            "read-only refusal hint (temp or Windows)"
        );
        // Same session under default stays read-only even when an active worker.
        db.set_code_session_permission_mode(&session.id, "default").unwrap();
        let l = prepare(&db, &dir, &db.get_code_session(&session.id).unwrap().unwrap()).unwrap();
        assert_eq!(
            l.env.iter().find(|(n, _)| n == "INITIAL_AGENT_MODE").map(|(_, v)| v.as_str()),
            Some("read-only")
        );
        db.set_code_session_permission_mode(&session.id, "auto").unwrap();

        // Item done (not yet released) => read-only: coding is finished; don't keep write sandbox.
        let conn = rusqlite::Connection::open(dir.join("teams.db")).unwrap();
        conn.execute("UPDATE items SET status = 'done' WHERE id = 'i1'", []).unwrap();
        assert!(!crate::teams::is_active_dispatched_worker_in(&dir, &session.id));
        assert!(crate::teams::is_dispatched_in(&dir, &session.id), "history row kept");
        let l = prepare(&db, &dir, &db.get_code_session(&session.id).unwrap().unwrap()).unwrap();
        assert_eq!(mode_of(&l), Some("read-only"), "item done => read-only");

        // Restore open item, then release membership: history remains, active flag clears.
        conn.execute("UPDATE items SET status = 'in-progress' WHERE id = 'i1'", []).unwrap();
        assert!(crate::teams::is_active_dispatched_worker_in(&dir, &session.id));
        conn.execute(
            "UPDATE members SET left_at = 1, status = 'released' WHERE session_id = ?1",
            [&session.id],
        )
        .unwrap();
        assert!(!crate::teams::is_active_dispatched_worker_in(&dir, &session.id));
        let l = prepare(&db, &dir, &db.get_code_session(&session.id).unwrap().unwrap()).unwrap();
        assert_eq!(mode_of(&l), Some("read-only"), "released => read-only");

        // Restart: new session is the active worker; under temp it still stays read-only (F2).
        let neu = db
            .create_code_session(&dir.to_string_lossy(), None, "codex", "auto")
            .unwrap();
        conn.execute(
            "INSERT INTO members(team_id, session_id, seat, role_id, status, item_id, joined_at)
             VALUES (?1, ?2, 'workers', 'builtin-worker', 'active', 'i1', 2)",
            rusqlite::params![team.id, neu.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatches(team_id, item_id, seat, desk_session, session_id, at)
             VALUES (?1, 'i1', 'workers', 'desk', ?2, 2)",
            rusqlite::params![team.id, neu.id],
        )
        .unwrap();
        assert!(crate::teams::is_active_dispatched_worker_in(&dir, &neu.id));
        assert!(!crate::teams::is_active_dispatched_worker_in(&dir, &session.id));
        assert_eq!(mode_of(&prepare(&db, &dir, &neu).unwrap()), Some("read-only"));
        assert_eq!(
            mode_of(&prepare(&db, &dir, &db.get_code_session(&session.id).unwrap().unwrap()).unwrap()),
            Some("read-only")
        );

        // A dispatches row for the same session but a different item_id does not count.
        let other = db
            .create_code_session(&dir.to_string_lossy(), None, "codex", "auto")
            .unwrap();
        conn.execute(
            "INSERT INTO items(team_id, id, title, type, severity, status, plan, created_at, updated_at)
             VALUES (?1, 'i2', 'i2', 'bug', 'low', 'in-progress', '', 0, 0)",
            [&team.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO members(team_id, session_id, seat, role_id, status, item_id, joined_at)
             VALUES (?1, ?2, 'workers', 'builtin-worker', 'active', 'i2', 3)",
            rusqlite::params![team.id, other.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatches(team_id, item_id, seat, desk_session, session_id, at)
             VALUES (?1, 'i1', 'workers', 'desk', ?2, 3)",
            rusqlite::params![team.id, other.id],
        )
        .unwrap();
        assert!(
            !crate::teams::is_active_dispatched_worker_in(&dir, &other.id),
            "dispatches.item_id must match membership.item_id"
        );
        assert_eq!(mode_of(&prepare(&db, &dir, &other).unwrap()), Some("read-only"));
        let _ = std::fs::remove_dir_all(&dir);

        fn mode_of(l: &Launch) -> Option<&str> {
            l.env.iter().find(|(n, _)| n == "INITIAL_AGENT_MODE").map(|(_, v)| v.as_str())
        }
    }

    /// Build-tree fixture root for F2 ALLOW cases.
    ///
    /// Must stay **outside** `/tmp`, `/private/tmp`, and `$TMPDIR` — F2 correctly refuses those
    /// for workspace-write. Prefer `CARGO_TARGET_DIR` (writable in CI/restricted sandboxes) over
    /// a hard-coded home path like `~/patchbay-teams-data`. Nested `git init` under target is fine
    /// for ALLOW (own repo). Temp refuse tests intentionally use `temp_dir` instead.
    fn f2_non_temp_fixture(id: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let root = base.join("f2-fixtures").join(id);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("trees")).unwrap();
        // Assert: ALLOW fixtures must never land under temp roots (CTD may be inside a worktree).
        if let (Some(key), Some(roots)) = (path_key(&root), temp_writable_roots()) {
            for r in &roots {
                assert!(
                    !path_under(&key, r),
                    "f2_non_temp_fixture must not sit under temp root {r:?} (got {key:?})"
                );
            }
        }
        root
    }

    /// Write an invalid gitfile so hardened `git rev-parse` fails regardless of parent ancestry /
    /// whether the path sits under CTD, a linked worktree, or a real checkout.
    fn f2_broken_git_dir(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let id = crate::db::new_id();
        std::fs::write(
            dir.join(".git"),
            format!("gitdir: /nonexistent/pb-f2-gitdir-{id}\n"),
        )
        .unwrap();
    }

    fn f2_git_init_commit(dir: &Path, file: &str, contents: &str) {
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.join(file), contents).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "add", "--", file])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "commit", "-m", "t"])
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        );
    }

    fn f2_add_worktree(repo: &Path, branch: &str, wt: &Path) {
        let wt_s = wt.to_string_lossy().into_owned();
        assert!(
            std::process::Command::new("git")
                .args(["worktree", "add", "-b", branch, "--", &wt_s, "main"])
                .current_dir(repo)
                .status()
                .unwrap()
                .success()
        );
    }

    /// Linked worktree under a non-temp build-tree path: active Codex worker Auto => workspace-write
    /// (Unix). On Windows the mapping always stays read-only until codex-acp sandboxing is proven.
    #[test]
    fn codex_dispatched_worker_auto_home_linked_worktree_is_workspace_write() {
        let id = crate::db::new_id();
        let home = f2_non_temp_fixture(&format!("proj-home-{id}"));
        // Main checkout + one linked worktree (common git dir stays outside the worktree cwd).
        f2_git_init_commit(&home, "README", "home");
        let wt = home.join("trees/i1");
        let wt_s = wt.to_string_lossy().into_owned();
        f2_add_worktree(&home, "i1", &wt);
        #[cfg(windows)]
        assert_eq!(
            codex_worker_workspace_write_blocked(&wt),
            Some(CODEX_WORKER_WINDOWS_READONLY_HINT)
        );
        #[cfg(not(windows))]
        assert!(codex_worker_workspace_write_blocked(&wt).is_none());

        let data = std::env::temp_dir().join(format!("pb-ag-home-{id}"));
        std::fs::create_dir_all(&data).unwrap();
        let db = std::sync::Arc::new(Db::open(&data.join("db.sqlite")).unwrap());
        let mut o = AgentPaths::new();
        o.insert("codex".into(), std::env::current_exe().unwrap().to_string_lossy().into_owned());
        crate::prefs::save(&db, &crate::prefs::Preferences { agent_paths: o, ..Default::default() }).unwrap();
        let session = db.create_code_session(&wt_s, None, "codex", "auto").unwrap();
        let tdb = crate::teams::TeamsDb::open(&data).unwrap();
        let team = tdb
            .create_team(crate::teams::NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "home".into(),
                cwd: home.to_string_lossy().into_owned(),
                id_prefix: Some("HM".into()),
                pr_base: Some("main".into()),
                worktree_root: Some(home.join("trees").to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(data.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        seed_active_worker(&data, &team.id, &session.id, "i1", "in-progress");
        let l = prepare(&db, &data, &session).unwrap();
        assert!(l.approvals.dispatched_worker);
        #[cfg(windows)]
        {
            assert_eq!(
                l.env.iter().find(|(n, _)| n == "INITIAL_AGENT_MODE").map(|(_, v)| v.as_str()),
                Some("read-only")
            );
            assert_eq!(l.approvals.select_mode.as_deref(), Some("read-only"));
            assert!(l.approvals.gaps.iter().any(|g| g.contains("Windows")));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                l.env.iter().find(|(n, _)| n == "INITIAL_AGENT_MODE").map(|(_, v)| v.as_str()),
                Some("workspace-write")
            );
            assert_eq!(l.approvals.select_mode.as_deref(), Some("workspace-write"));
            assert_eq!(l.approvals.gaps, codex_gaps_for(Some("auto"), true, Some(Path::new(&wt_s))));
            assert!(!l.approvals.gaps.iter().any(|g| *g == CODEX_WORKER_TEMP_READONLY_HINT));
        }

        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", "--", &wt_s])
            .current_dir(&home)
            .status();
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Expected F2 refuse hint: Windows always uses the Windows constant; elsewhere the temp/layout hint.
    fn f2_refuse_hint() -> &'static str {
        #[cfg(windows)]
        {
            CODEX_WORKER_WINDOWS_READONLY_HINT
        }
        #[cfg(not(windows))]
        {
            CODEX_WORKER_TEMP_READONLY_HINT
        }
    }

    /// F2 gate: temp refuse; main-checkout refuse; linked non-temp allow; edge layouts.
    #[test]
    fn codex_worker_workspace_write_blocked_cases() {
        let temp = std::env::temp_dir().join(format!("pb-f2-temp-{}", crate::db::new_id()));
        std::fs::create_dir_all(&temp).unwrap();
        f2_git_init_commit(&temp, "f", "x");
        assert_eq!(
            codex_worker_workspace_write_blocked(&temp),
            Some(f2_refuse_hint()),
            "temp => blocked"
        );
        let _ = std::fs::remove_dir_all(&temp);

        // Relative $TMPDIR cannot be compared safely => refuse (unit on roots helper).
        assert!(
            temp_writable_roots_from(Some("relative-tmpdir")).is_none(),
            "relative TMPDIR => roots unavailable => mapping refused"
        );
        #[cfg(unix)]
        assert!(temp_writable_roots_from(Some("/tmp")).is_some());
        #[cfg(windows)]
        {
            let temp = std::env::temp_dir();
            let temp_s = temp.to_str().expect("temp_dir utf-8");
            assert!(
                temp_writable_roots_from(Some(temp_s)).is_some(),
                "absolute Windows temp as TMPDIR => ok"
            );
        }
        // Whitespace-only / padded $TMPDIR must not trim-to-unset — refuse unless raw is absolute.
        assert!(
            temp_writable_roots_from(Some("   ")).is_none(),
            "whitespace-only TMPDIR => refuse (no trim-to-unset)"
        );
        assert!(
            temp_writable_roots_from(Some("\t\n")).is_none(),
            "tab/newline TMPDIR => refuse"
        );
        assert!(
            temp_writable_roots_from(Some("  /tmp  ")).is_none(),
            "padded absolute TMPDIR is not absolute as raw Path => refuse"
        );
        assert!(
            temp_writable_roots_from(Some("")).is_some(),
            "empty string TMPDIR treated as unset => ok"
        );
        assert!(temp_writable_roots_from(None).is_some());

        let id = crate::db::new_id();
        let home = f2_non_temp_fixture(&format!("proj-gate-{id}"));
        f2_git_init_commit(&home, "README", "x");
        assert_eq!(
            codex_worker_workspace_write_blocked(&home),
            Some(f2_refuse_hint()),
            "main-checkout cwd => blocked (git metadata under cwd)"
        );
        let wt = home.join("trees/wt1");
        let wt_s = wt.to_string_lossy().into_owned();
        f2_add_worktree(&home, "wt1", &wt);
        #[cfg(windows)]
        assert_eq!(
            codex_worker_workspace_write_blocked(&wt),
            Some(CODEX_WORKER_WINDOWS_READONLY_HINT),
            "Windows always refuses"
        );
        #[cfg(not(windows))]
        assert_eq!(
            codex_worker_workspace_write_blocked(&wt),
            None,
            "non-temp linked worktree => allowed"
        );
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", "--", &wt_s])
            .current_dir(&home)
            .status();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Project under the system temp root is refused; on unix also `/TMP` case-folding vs `/tmp`.
    #[test]
    fn codex_worker_workspace_write_blocked_tmp_case_spelling() {
        let id = crate::db::new_id();
        let under_tmp = std::env::temp_dir().join(format!("pb-f2-case-{id}"));
        let _ = std::fs::remove_dir_all(&under_tmp);
        std::fs::create_dir_all(&under_tmp).unwrap();
        f2_git_init_commit(&under_tmp, "f", "x");
        assert_eq!(
            codex_worker_workspace_write_blocked(&under_tmp),
            Some(f2_refuse_hint()),
            "system temp path => blocked"
        );
        // Differently-cased spelling: on case-insensitive volumes this is the same inode; after
        // canonicalize it still sits under the temp root. If the volume is case-sensitive and
        // `/TMP` does not resolve, path_key / git probe fails closed → still refuse.
        #[cfg(unix)]
        {
            let under_slash_tmp = PathBuf::from(format!("/tmp/pb-f2-case-{id}"));
            let _ = std::fs::remove_dir_all(&under_slash_tmp);
            std::fs::create_dir_all(&under_slash_tmp).unwrap();
            f2_git_init_commit(&under_slash_tmp, "f", "x");
            assert_eq!(
                codex_worker_workspace_write_blocked(&under_slash_tmp),
                Some(CODEX_WORKER_TEMP_READONLY_HINT),
                "/tmp path => blocked"
            );
            let weird = PathBuf::from(format!("/TMP/pb-f2-case-{id}"));
            assert_eq!(
                codex_worker_workspace_write_blocked(&weird),
                Some(CODEX_WORKER_TEMP_READONLY_HINT),
                "/TMP case spelling => blocked (canonicalize or fail-closed)"
            );
            let _ = std::fs::remove_dir_all(&under_slash_tmp);
        }
        let _ = std::fs::remove_dir_all(&under_tmp);
    }

    #[test]
    fn codex_worker_workspace_write_blocked_separate_git_dir() {
        let id = crate::db::new_id();
        let base = f2_non_temp_fixture(&format!("sep-{id}"));
        let main = base.join("main");
        let gitdir = base.join("gitdir");
        std::fs::create_dir_all(&main).unwrap();
        let gitdir_s = gitdir.to_string_lossy().into_owned();
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "--separate-git-dir", &gitdir_s])
                .current_dir(&main)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(main.join("f"), "x").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "add", "f"])
                .current_dir(&main)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "commit", "-m", "t"])
                .current_dir(&main)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        );
        // Separate git dir outside cwd must not allow workspace-write on the main checkout.
        assert_eq!(
            codex_worker_workspace_write_blocked(&main),
            Some(f2_refuse_hint()),
            "separate-git-dir main checkout => blocked"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn codex_worker_workspace_write_blocked_submodule() {
        let id = crate::db::new_id();
        let base = f2_non_temp_fixture(&format!("submod-{id}"));
        let super_repo = base.join("super");
        let other = base.join("other");
        std::fs::create_dir_all(&super_repo).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        f2_git_init_commit(&other, "f", "sub");
        f2_git_init_commit(&super_repo, "README", "super");
        let other_s = other.to_string_lossy().into_owned();
        assert!(
            std::process::Command::new("git")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "--",
                    &other_s,
                    "sub",
                ])
                .current_dir(&super_repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success()
        );
        let sub = super_repo.join("sub");
        assert_eq!(
            codex_worker_workspace_write_blocked(&sub),
            Some(f2_refuse_hint()),
            "submodule cwd => blocked"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn codex_worker_workspace_write_blocked_bare_worktree() {
        let id = crate::db::new_id();
        let base = f2_non_temp_fixture(&format!("bare-{id}"));
        let seed = base.join("seed");
        let bare = base.join("bare.git");
        let wt = base.join("wt");
        std::fs::create_dir_all(&seed).unwrap();
        f2_git_init_commit(&seed, "f", "x");
        let bare_s = bare.to_string_lossy().into_owned();
        assert!(
            std::process::Command::new("git")
                .args(["clone", "--bare", "--", &seed.to_string_lossy(), &bare_s])
                .status()
                .unwrap()
                .success()
        );
        let wt_s = wt.to_string_lossy().into_owned();
        assert!(
            std::process::Command::new("git")
                .args(["worktree", "add", "-b", "w1", "--", &wt_s, "main"])
                .current_dir(&bare)
                .status()
                .unwrap()
                .success()
        );
        // Linked worktree of a non-temp bare: common/main live outside the worktree cwd.
        #[cfg(windows)]
        assert_eq!(
            codex_worker_workspace_write_blocked(&wt),
            Some(CODEX_WORKER_WINDOWS_READONLY_HINT),
            "Windows always refuses"
        );
        #[cfg(not(windows))]
        assert_eq!(
            codex_worker_workspace_write_blocked(&wt),
            None,
            "bare+linked worktree outside temp => allowed"
        );
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", "--", &wt_s])
            .current_dir(&bare)
            .status();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn codex_worker_workspace_write_blocked_probe_failure() {
        let id = crate::db::new_id();
        // Broken gitfile under f2_non_temp_fixture (INSIDE target/CTD — even a linked worktree).
        // Invalid gitdir makes hardened rev-parse fail; must not depend on orphan-temp location.
        let base = f2_non_temp_fixture(&format!("broken-git-{id}"));
        let broken = base.join("orphan");
        f2_broken_git_dir(&broken);
        assert_eq!(
            codex_worker_workspace_write_blocked(&broken),
            Some(f2_refuse_hint()),
            "invalid gitfile => refuse (location-independent)"
        );

        // Real linked worktree, but git binary missing => probe fails closed.
        let repo = f2_non_temp_fixture(&format!("nogit-{id}"));
        f2_git_init_commit(&repo, "f", "x");
        let wt = repo.join("trees/w");
        f2_add_worktree(&repo, "w", &wt);
        F2_GIT_PROGRAM_OVERRIDE.with(|c| {
            *c.borrow_mut() = Some(PathBuf::from("/nonexistent/git-for-f2-probe"))
        });
        let blocked = codex_worker_workspace_write_blocked(&wt);
        F2_GIT_PROGRAM_OVERRIDE.with(|c| *c.borrow_mut() = None);
        assert_eq!(
            blocked,
            Some(f2_refuse_hint()),
            "git missing => refuse"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Linked worktree + subdirectory cwd (not worktree root).
    ///
    /// Product dispatch always uses `worktree_root.join(item_id.to_lowercase())` — never a
    /// subdirectory — so this is not a dispatch path. Gate still evaluates the subdirectory as
    /// cwd: same linked-worktree topology (common/main outside), non-temp → currently ALLOW.
    /// Writable root would be that subdirectory if a mis-set cwd ever reached prepare.
    #[test]
    fn codex_worker_workspace_write_blocked_linked_worktree_subdirectory() {
        let id = crate::db::new_id();
        let home = f2_non_temp_fixture(&format!("proj-subdir-{id}"));
        f2_git_init_commit(&home, "README", "x");
        let wt = home.join("trees/wt1");
        let wt_s = wt.to_string_lossy().into_owned();
        f2_add_worktree(&home, "wt1", &wt);
        let sub = wt.join("nested/deep");
        std::fs::create_dir_all(&sub).unwrap();
        #[cfg(windows)]
        assert_eq!(
            codex_worker_workspace_write_blocked(&sub),
            Some(CODEX_WORKER_WINDOWS_READONLY_HINT),
            "Windows always refuses"
        );
        #[cfg(not(windows))]
        assert_eq!(
            codex_worker_workspace_write_blocked(&sub),
            None,
            "linked worktree subdirectory (non-temp) => currently ALLOW"
        );
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force", "--", &wt_s])
            .current_dir(&home)
            .status();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A team member's role instructions reach the prompt hook of each agent that has one, and
    /// no launch for an agent without one (those get a preamble, see `teams::preamble_for`).
    #[test]
    fn team_instructions_ride_the_prompt_hooks() {
        let dir = std::env::temp_dir().join(format!("pb-ag-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = std::sync::Arc::new(Db::open(&dir.join("db.sqlite")).unwrap());
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let mut o = AgentPaths::new();
        for id in ["claude-code", "antigravity", "codex", "opencode"] {
            o.insert(id.into(), exe.clone());
        }
        let prefs = crate::prefs::Preferences { agent_paths: o, agent_instructions: "be terse".into(), ..Default::default() };
        crate::prefs::save(&db, &prefs).unwrap();
        let tdb = crate::teams::TeamsDb::open(&dir).unwrap();
        let team = tdb
            .create_team(crate::teams::NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "t".into(),
                cwd: dir.to_string_lossy().into_owned(),
                id_prefix: None,
                pr_base: None,
                worktree_root: None,
                project_rules: None,
                export_dir: Some(format!("{}-out", dir.display())),
            })
            .unwrap();
        let mut launch = |agent: &str, seat: &str| {
            let s = db.create_code_session(&dir.to_string_lossy(), None, agent, "default").unwrap();
            tdb.attach(&team.id, &s.id, seat).unwrap();
            prepare(&db, &dir, &s).unwrap()
        };
        let desk = "You are the coordinator";

        let l = launch("claude-code", "coordinator");
        let append = l.session_meta["systemPrompt"]["append"].as_str().unwrap().to_owned();
        assert!(append.starts_with("be terse") && append.contains(desk), "{append}");
        let l = launch("antigravity", "advisor");
        let append = l.session_meta["antigravity"]["systemPrompt"]["append"].as_str().unwrap().to_owned();
        assert!(append.starts_with("be terse") && append.contains("You are the advisor"), "{append}");
        let l = launch("codex", "lead");
        let cfg = l.env.iter().find(|(k, _)| k == "CODEX_CONFIG").unwrap().1.clone();
        let cfg: Value = serde_json::from_str(&cfg).unwrap();
        assert!(cfg["developer_instructions"].as_str().unwrap().contains("You are the ship lead"));

        let l = launch("opencode", "workers");
        assert_eq!(l.session_meta, json!({}));
        assert!(l.env.iter().all(|(k, v)| !v.contains("You are a worker") && k != "CODEX_CONFIG"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acp_arguments_per_agent() {
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let args = |kind: AgentKind| {
            let mut o = AgentPaths::new();
            o.insert(kind.id().into(), exe.clone());
            resolve(kind, &o).unwrap().args
        };
        assert_eq!(args(AgentKind::Codex), Vec::<String>::new());
        let agy = args(AgentKind::Antigravity);
        assert_eq!(
            agy,
            if cfg!(target_os = "linux") {
                vec!["--uid=".to_owned()]
            } else {
                vec![]
            }
        );
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

    #[test]
    fn cursor_preload_keeps_the_users_node_options() {
        let dir = std::env::temp_dir().join(format!("pb ag {}", crate::db::new_id()));
        let env = cursor_preload(&dir, Some("--max-old-space-size=4096".into()));
        let path = dir.join("cursor").join("acp-preload.cjs");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CURSOR_PRELOAD);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let quoted = path.to_string_lossy().replace('\\', "\\\\");
        assert_eq!(
            get("NODE_OPTIONS").unwrap(),
            format!("--require \"{quoted}\" --max-old-space-size=4096")
        );
        assert_eq!(
            get("PATCHBAY_ORIG_NODE_OPTIONS").unwrap(),
            "--max-old-space-size=4096"
        );
        // Without the user's own options; and again over the file already there.
        let env = cursor_preload(&dir, None);
        assert_eq!(env[0].1, format!("--require \"{quoted}\""));
        assert_eq!(env[1].1, "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interactive_cli_is_detected_and_rejected() {
        assert!(is_interactive_cli(Path::new("/Users/me/.local/bin/agy")));
        assert!(is_interactive_cli(Path::new("C:\\Users\\me\\bin\\agy.exe")));
        assert!(!is_interactive_cli(Path::new(
            "/Users/me/bin/agy_acp_server"
        )));
        assert!(!is_interactive_cli(Path::new(
            "/Users/me/bin/agy_acp_server.par"
        )));

        let mut o = AgentPaths::new();
        let tmp_dir = std::env::temp_dir().join(format!("agy-test-{}", crate::db::new_id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp = tmp_dir.join("agy");
        std::fs::write(&tmp, b"dummy").unwrap();
        o.insert("antigravity".into(), tmp.to_string_lossy().into_owned());
        let err = resolve(AgentKind::Antigravity, &o).unwrap_err().to_string();
        let _ = std::fs::remove_dir_all(&tmp_dir);
        assert!(err.contains("interactive CLI"), "expected CLI error: {err}");
    }

    #[test]
    fn antigravity_release_exists_for_supported_platforms() {
        let release = antigravity_release();
        assert!(release.is_some());
        let r = release.unwrap();
        assert!(
            r.url
                .starts_with("https://dl.google.com/agy-extensions/releases/")
        );
        assert!(!r.sha256.is_empty());
    }

    #[test]
    fn antigravity_auth_status_parses_settings_and_clears() {
        // Holds GEMINI_HOME for the whole test: another test swaps the same variable.
        let _env = crate::test_support::gemini_home_lock();
        let dir = std::env::temp_dir().join(format!("gemini-test-{}", crate::db::new_id()));
        let settings = dir.join("antigravity-acp").join("settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, br#"{"auth": {"type": "oauth-personal"}}"#).unwrap();

        unsafe { std::env::set_var("GEMINI_HOME", &dir) };
        assert_eq!(
            antigravity_auth_status(),
            Some("Google Account (Personal)".to_owned())
        );

        clear_antigravity_auth().unwrap();
        assert_eq!(antigravity_auth_status(), None);

        unsafe { std::env::remove_var("GEMINI_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
