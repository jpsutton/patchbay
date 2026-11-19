//! Bash, and the background shells BashOutput and KillShell manage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use anyhow::{Context, Result, anyhow};
use patchbay_llm::ToolSpec;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::session::Session;

const MAX_TIMEOUT_MS: u64 = 600_000;
const MODEL_OUTPUT_CHARS: usize = 30_000;
const BUFFER_CAP: usize = 2 * 1024 * 1024;
/// The longest BashOutput waits for a background command to end.
const MAX_WAIT_SECS: u64 = 120;
/// A child that cannot be confirmed stopped must not hold session shutdown forever.
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound the complete native-agent shell cleanup, including multiple running jobs.
const KILL_ALL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(unix)]
const GROUP_STOP_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const GROUP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// A running command.
pub struct Running {
    buf: Arc<Mutex<String>>,
    pid: Option<u32>,
    /// `Some(code)` once it ended (`-1` when killed or without a code).
    done: watch::Receiver<Option<i32>>,
    kill: CancellationToken,
}

impl Running {
    fn output(&self) -> String {
        self.buf.lock().unwrap().clone()
    }
}

struct Background {
    run: Running,
    /// How much of the output BashOutput has already returned.
    read: Mutex<usize>,
}

#[derive(Default)]
pub struct Registry {
    shells: Mutex<HashMap<String, Arc<Background>>>,
    next: AtomicU64,
    /// Where commands run next: a `cd` in one command carries to the next.
    cwd: Mutex<Option<PathBuf>>,
    /// Woken to move the foreground commands that are running to the background.
    to_background: tokio::sync::Notify,
}

impl Registry {
    pub fn cwd(&self, fallback: &Path) -> PathBuf {
        self.cwd.lock().unwrap().clone().filter(|p| p.is_dir()).unwrap_or_else(|| fallback.to_path_buf())
    }

    /// Stop one background command; false when there is none with this id.
    pub fn kill(&self, id: &str) -> bool {
        match self.shells.lock().unwrap().get(id) {
            Some(b) => {
                b.run.kill.cancel();
                true
            }
            None => false,
        }
    }

    /// Ask every foreground Bash call that is running to move its command to the background.
    pub fn background_foreground(&self) {
        self.to_background.notify_waiters();
    }

    /// Stop every background command (the session is closing).
    pub async fn kill_all(&self) -> Result<()> {
        self.kill_all_with_timeout(KILL_ALL_TIMEOUT).await
    }

    async fn kill_all_with_timeout(&self, timeout: Duration) -> Result<()> {
        self.kill_all_with_budgets(timeout, CHILD_STOP_TIMEOUT).await
    }

    async fn kill_all_with_budgets(&self, timeout: Duration, done_timeout: Duration) -> Result<()> {
        let mut done = Vec::new();
        let mut groups = Vec::new();
        for b in self.shells.lock().unwrap().values() {
            b.run.kill.cancel();
            done.push(b.run.done.clone());
            groups.push(b.run.pid);
        }
        tokio::time::timeout(timeout, async move {
            let mut first_error = None;
            for pgid in groups {
                if let Err(error) = terminate_tree(pgid).await {
                    if first_error.is_none() { first_error = Some(error); }
                }
            }
            if let Err(error) = wait_done_receivers_with(done, done_timeout).await {
                if first_error.is_none() { first_error = Some(error); }
            }
            if let Some(error) = first_error { return Err(error); }
            Ok(())
        }).await.map_err(|_| anyhow!("native shell shutdown exceeded its deadline"))?
    }
}

fn shell_program() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else if which_bash() {
        ("bash", "-c")
    } else {
        ("sh", "-c")
    }
}

fn which_bash() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bash").is_file()))
        .unwrap_or(false)
}

/// Start `command`. Output lands in the returned buffer and, when given, goes to `on_chunk`.
fn spawn_shell(
    command: &str,
    dir: &Path,
    cwd_file: Option<PathBuf>,
    on_chunk: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> Result<Running, String> {
    let (program, flag) = shell_program();
    let script = match (&cwd_file, cfg!(windows)) {
        (Some(f), false) => format!("{command}\n__pb_rc=$?\npwd -P > '{}' 2>/dev/null\nexit $__pb_rc", f.display()),
        _ => command.to_owned(),
    };
    let mut cmd = tokio::process::Command::new(program);
    crate::proc::apply_script_args(&mut cmd, flag, &script)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .kill_on_drop(true);
    // The same PATH as the ACP agents get: a desktop launch's bare one finds no user tools.
    if let Some(path) = crate::proc::child_path(None) {
        cmd.env("PATH", path);
    }
    crate::proc::NoWindow::no_window(&mut cmd);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("Cannot start the shell: {e}"))?;
    let pid = child.id();
    let buf = Arc::new(Mutex::new(String::new()));
    let (done_tx, done_rx) = watch::channel(None);
    let kill = CancellationToken::new();

    let mut readers = Vec::new();
    for stream in [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let buf = buf.clone();
        let on_chunk = on_chunk.clone();
        readers.push(tokio::spawn(async move {
            let mut stream = stream;
            let mut chunk = vec![0u8; 8192];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&chunk[..n]);
                        // Hold back an incomplete UTF-8 sequence for the next read.
                        let valid = match std::str::from_utf8(&pending) {
                            Ok(_) => pending.len(),
                            // An incomplete tail: wait for the rest of the character.
                            Err(e) if e.error_len().is_none() => e.valid_up_to(),
                            Err(_) => pending.len(),
                        };
                        if valid == 0 {
                            continue;
                        }
                        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        pending.drain(..valid);
                        {
                            let mut b = buf.lock().unwrap();
                            if b.len() < BUFFER_CAP {
                                b.push_str(&text);
                            }
                        }
                        if let Some(f) = &on_chunk {
                            f(&text);
                        }
                    }
                }
            }
        }));
    }

    let kill2 = kill.clone();
    tokio::spawn(async move {
        let code = tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(-1),
            _ = kill2.cancelled() => {
                let stop_result = terminate_tree(pid).await;
                let kill_result = child.start_kill().map_err(anyhow::Error::from);
                let wait_result = wait_for_child_exit_with(kill_result, child.wait(), CHILD_STOP_TIMEOUT).await;
                if stop_result.is_ok() && wait_result.is_ok() { -1 } else { -2 }
            }
        };
        // Output the process left behind in the pipes still counts.
        let _ = tokio::time::timeout(Duration::from_millis(500), futures::future::join_all(readers)).await;
        let _ = done_tx.send(Some(code));
    });
    Ok(Running { buf, pid, done: done_rx, kill })
}

#[cfg(unix)]
async fn terminate_tree(pid: Option<u32>) -> Result<()> {
    if let Some(pid) = pid {
        // The command leads its own process group. Give children a brief graceful stop before
        // escalating; the session close does not complete until this task reaps the leader.
        ensure_safe_group(pid)?;
        signal_shell_group(pid, libc::SIGTERM).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        signal_shell_group(pid, libc::SIGKILL).await?;
        wait_shell_group_exit_with(
            tokio::time::Instant::now() + GROUP_STOP_TIMEOUT,
            || shell_group_has_live_members(pid),
        ).await?;
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_safe_group(pgid: u32) -> Result<()> {
    let own_pgid = unsafe { libc::getpgrp() as u32 };
    if pgid <= 1 || pgid == own_pgid {
        return Err(anyhow!("refusing unsafe shell process group target {pgid}"));
    }
    Ok(())
}

#[cfg(unix)]
async fn signal_shell_group(pgid: u32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(-(pgid as i32), signal) };
    if result == 0 { return Ok(()); }
    let error = std::io::Error::last_os_error();
    resolve_shell_signal_error(error, || shell_group_has_live_members(pgid)).await
}

#[cfg(unix)]
async fn resolve_shell_signal_error<F, Fut>(error: std::io::Error, probe: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        Some(libc::EPERM) => match probe().await {
            Ok(false) => Ok(()),
            Ok(true) => Err(error).map_err(Into::into),
            Err(probe_error) => Err(probe_error),
        },
        _ => Err(error).map_err(Into::into),
    }
}

#[cfg(unix)]
async fn wait_shell_group_exit_with<F, Fut>(deadline: tokio::time::Instant, mut probe: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    loop {
        if !probe().await? { return Ok(()); }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("shell process group remained live after termination deadline"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
async fn shell_group_has_live_members(pgid: u32) -> Result<bool> {
    let output = tokio::time::timeout(
        GROUP_PROBE_TIMEOUT,
        tokio::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pgid=", "-o", "stat="])
        .kill_on_drop(true)
        .output(),
    ).await.map_err(|_| anyhow!("shell process-group probe timed out"))??;
    if !output.status.success() {
        return Err(anyhow!("shell process-group probe failed: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(group) = fields.next().and_then(|v| v.parse::<u32>().ok()) else {
            return false;
        };
        let Some(state) = fields.next() else {
            return false;
        };
        group == pgid && !state.starts_with('Z') && !state.starts_with('X')
    }))
}

#[cfg(not(unix))]
async fn terminate_tree(_pid: Option<u32>) -> Result<()> { Ok(()) }

async fn finished(run: &mut Running) -> i32 {
    wait_done(run.done.clone()).await
}

/// The exit code once the command ended.
async fn wait_done(mut done: watch::Receiver<Option<i32>>) -> i32 {
    loop {
        if let Some(c) = *done.borrow() {
            return c;
        }
        if done.changed().await.is_err() {
            return -1;
        }
    }
}

async fn wait_for_child_exit_with<F, T>(
    kill_result: Result<()>,
    wait: F,
    timeout: Duration,
) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(error).context("wait for stopped shell child"),
        Err(_) => match kill_result {
            Err(error) => Err(error).context("shell child did not exit after kill failed"),
            Ok(()) => Err(anyhow!("shell child did not exit before the stop deadline")),
        },
    }
}

async fn wait_done_receivers_with(
    receivers: Vec<watch::Receiver<Option<i32>>>,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    for receiver in receivers {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let exit_code = tokio::time::timeout(remaining, wait_done(receiver))
            .await
            .map_err(|_| anyhow!("native shell did not report stopped before the deadline"))?;
        if exit_code == -2 {
            return Err(anyhow!("native shell process cleanup failed; refusing session reuse"));
        }
    }
    Ok(())
}

// --- Bash ---

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Run a shell command and return its output (stdout and stderr together). The \
working directory carries over between calls; other shell state does not. Use it for builds, tests, git \
and similar, not for reading, searching or editing files: the dedicated tools do that better. \
Quote paths with spaces. Commands time out after 2 minutes unless you set `timeout` (milliseconds, \
at most 600000). Set `run_in_background` for servers and slow jobs, then use BashOutput and KillShell. \
Do not wait with `sleep`: a fixed sleep of more than 30 seconds is refused. Wait for a background \
job with BashOutput and `wait_seconds`. To find a process, put square brackets around the last \
character of the pattern (`pgrep -f '[f]oo'`, `ps aux | grep '[f]oo'`) so the search does not match \
itself; a pattern that does is refused."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "description": {"type": "string", "description": "Plain-English summary of what this command does and why, in 5-10 words (for example \"Run the unit tests\" or \"Watch CI for PR 198\"). The person watching reads this instead of the command."},
                    "timeout": {"type": "integer", "description": "Timeout in milliseconds (max 600000)"},
                    "run_in_background": {"type": "boolean", "description": "Run in the background and return an id"}
                },
                "required": ["command", "description"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "execute"
    }

    fn allowed_in_plan(&self) -> bool {
        false
    }

    fn title(&self, input: &Value) -> String {
        match str_arg(input, "description").filter(|d| !d.is_empty()) {
            Some(d) => d.to_owned(),
            None => format!("`{}`", clip_middle(str_arg(input, "command").unwrap_or(""), 120)),
        }
    }

    fn needs_permission(&self, input: &Value, _: &Session) -> bool {
        // A command the lint refuses never runs, so there is nothing to approve.
        match str_arg(input, "command") {
            Some(c) => shell_lint::check(c, bool_arg(input, "run_in_background")).is_none(),
            None => true,
        }
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(command) = str_arg(&input, "command").filter(|c| !c.trim().is_empty()) else {
            return ToolOutput::error("command is required");
        };
        if let Some(problem) = shell_lint::check(command, bool_arg(&input, "run_in_background")) {
            return ToolOutput::error(problem.message(ctx.session.prompts()));
        }
        let session = &ctx.session;
        let dir = session.shells.cwd(&session.cwd);
        if bool_arg(&input, "run_in_background") {
            return start_background(session, &ctx.call_id, command, &dir);
        }
        let timeout_ms = u64_arg(&input, "timeout").unwrap_or(ctx.session.host.settings().limits.shell_timeout_ms).clamp(1, MAX_TIMEOUT_MS);
        let cwd_file = std::env::temp_dir().join(format!("pb-cwd-{}", uuid::Uuid::new_v4()));
        let stream_ctx = (ctx.session.clone(), ctx.call_id.clone());
        let on_chunk: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |t: &str| {
            stream_ctx.0.update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": stream_ctx.1,
                "_meta": {"terminal_output_delta": {"data": t}},
            }));
        });
        let mut run = match spawn_shell(command, &dir, Some(cwd_file.clone()), Some(on_chunk)) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(e),
        };
        let outcome = tokio::select! {
            code = finished(&mut run) => Ok(code),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Err("timeout"),
            _ = ctx.cancel.cancelled() => Err("cancelled"),
            _ = session.shells.to_background.notified() => Err("background"),
        };
        let code = match outcome {
            Ok(c) => c,
            // The user sent a message: the command keeps running, as a background one, and the
            // turn goes on. Its `cd` is lost.
            Err("background") => {
                let _ = std::fs::remove_file(&cwd_file);
                // What it printed so far goes to the model now, so BashOutput starts after it.
                let so_far = run.output();
                let id = adopt(session, &ctx.call_id, run, so_far.len());
                let mut text = clip_middle(so_far.trim_end(), MODEL_OUTPUT_CHARS);
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!(
                    "Command moved to the background with ID: {id}. Read its output with BashOutput, stop it with KillShell."
                ));
                let mut out = ToolOutput::text(text);
                out.meta = Some(json!({"background_shell": {"id": id}}));
                return out;
            }
            Err(why) => {
                run.kill.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(2), finished(&mut run)).await;
                let mut text = clip_middle(run.output().trim_end(), MODEL_OUTPUT_CHARS);
                let note = if why == "timeout" {
                    format!("Command timed out after {}s.", timeout_ms / 1000)
                } else {
                    "Command was interrupted.".to_owned()
                };
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&note);
                let mut out = ToolOutput::error(text);
                out.meta = Some(json!({"terminal_exit": {"exit_code": null}}));
                return out;
            }
        };
        if let Ok(new_dir) = std::fs::read_to_string(&cwd_file) {
            let new_dir = PathBuf::from(new_dir.trim());
            if new_dir.is_dir() {
                *session.shells.cwd.lock().unwrap() = Some(new_dir);
            }
        }
        let _ = std::fs::remove_file(&cwd_file);
        let mut text = clip_middle(run.output().trim_end(), MODEL_OUTPUT_CHARS);
        if code != 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("Exit code {code}"));
        }
        let mut out = if code == 0 { ToolOutput::text(text) } else { ToolOutput::error(text) };
        out.raw = json!({"exit_code": code});
        out.meta = Some(json!({"terminal_exit": {"exit_code": code}}));
        out
    }
}

/// Register a running command as a background one: the client hears when it ends, on the card
/// of the call that started it. Returns its id.
fn adopt(session: &Arc<Session>, call_id: &str, run: Running, read: usize) -> String {
    let id = format!("bash_{}", session.shells.next.fetch_add(1, Ordering::Relaxed) + 1);
    let done = run.done.clone();
    let (watcher, card, job) = (session.clone(), call_id.to_owned(), id.clone());
    tokio::spawn(async move {
        let code = wait_done(done).await;
        watcher.update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": card,
            "_meta": {"background_shell": {"id": job, "done": true, "exit_code": code}},
        }));
    });
    session
        .shells
        .shells
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::new(Background { run, read: Mutex::new(read) }));
    id
}

fn start_background(session: &Arc<Session>, call_id: &str, command: &str, dir: &Path) -> ToolOutput {
    let run = match spawn_shell(command, dir, None, None) {
        Ok(r) => r,
        Err(e) => return ToolOutput::error(e),
    };
    let id = adopt(session, call_id, run, 0);
    let mut out = ToolOutput::text(format!(
        "Command running in background with ID: {id}. Read its output with BashOutput, stop it with KillShell."
    ));
    out.meta = Some(json!({"background_shell": {"id": id}}));
    out
}

// --- BashOutput ---

pub struct BashOutput;

#[async_trait]
impl Tool for BashOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "BashOutput".into(),
            description: "Read the output a background command (started with Bash `run_in_background`) \
has produced since you last read it, and whether it still runs. Optional `filter` is a regular \
expression: only matching lines come back. Set `wait_seconds` (at most 120) to wait for the command \
to end before reading, instead of sleeping."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "The id Bash returned"},
                    "filter": {"type": "string", "description": "Regular expression for the lines to keep"},
                    "wait_seconds": {"type": "integer", "description": "Wait up to this many seconds (max 120) for the command to end before reading its output"}
                },
                "required": ["bash_id"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Read output of {}", str_arg(input, "bash_id").unwrap_or("background command"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(id) = str_arg(&input, "bash_id") else { return ToolOutput::error("bash_id is required") };
        let Some(bg) = ctx.session.shells.shells.lock().unwrap().get(id).cloned() else {
            return ToolOutput::error(format!("No background command with ID {id}."));
        };
        let wait = u64_arg(&input, "wait_seconds").unwrap_or(0).min(MAX_WAIT_SECS);
        if wait > 0 && bg.run.done.borrow().is_none() {
            tokio::select! {
                _ = wait_done(bg.run.done.clone()) => {}
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                _ = ctx.cancel.cancelled() => {}
            }
        }
        let all = bg.run.output();
        let mut read = bg.read.lock().unwrap();
        let start = (*read).min(all.len());
        let mut start = start;
        while !all.is_char_boundary(start) {
            start += 1;
        }
        let new = all[start..].to_owned();
        *read = all.len();
        drop(read);
        let new = match str_arg(&input, "filter").filter(|f| !f.is_empty()) {
            Some(f) => match Regex::new(f) {
                Ok(re) => new.lines().filter(|l| re.is_match(l)).collect::<Vec<_>>().join("\n"),
                Err(e) => return ToolOutput::error(format!("invalid filter: {e}")),
            },
            None => new,
        };
        let status = match *bg.run.done.borrow() {
            None => "running".to_owned(),
            Some(-1) => "stopped".to_owned(),
            Some(c) => format!("completed (exit code {c})"),
        };
        ToolOutput::text(format!("<status>{status}</status>\n{}", clip_middle(new.trim_end(), MODEL_OUTPUT_CHARS)))
    }
}

// --- KillShell ---

pub struct KillShell;

#[async_trait]
impl Tool for KillShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "KillShell".into(),
            description: "Stop a background command started with Bash `run_in_background`.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"shell_id": {"type": "string", "description": "The id Bash returned"}},
                "required": ["shell_id"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "execute"
    }

    fn allowed_in_plan(&self) -> bool {
        false
    }

    fn title(&self, input: &Value) -> String {
        format!("Stop {}", str_arg(input, "shell_id").unwrap_or("background command"))
    }

    fn needs_permission(&self, _: &Value, _: &Session) -> bool { true }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(id) = str_arg(&input, "shell_id") else { return ToolOutput::error("shell_id is required") };
        match ctx.session.shells.shells.lock().unwrap().get(id) {
            Some(bg) => {
                bg.run.kill.cancel();
                ToolOutput::text(format!("Stopped {id}."))
            }
            None => ToolOutput::error(format!("No background command with ID {id}.")),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_a_command_and_reports_the_exit_code() {
        let dir = std::env::temp_dir();
        let mut run = spawn_shell("echo out; echo err >&2; exit 3", &dir, None, None).unwrap();
        assert_eq!(finished(&mut run).await, 3);
        let out = run.output();
        assert!(out.contains("out") && out.contains("err"), "{out}");
    }

    #[tokio::test]
    async fn kill_stops_the_whole_tree() {
        let dir = std::env::temp_dir();
        let mut run = spawn_shell("sleep 30 & sleep 30", &dir, None, None).unwrap();
        run.kill.cancel();
        let code = tokio::time::timeout(Duration::from_secs(3), finished(&mut run)).await.expect("stopped");
        assert_eq!(code, -1);
    }

    #[tokio::test]
    async fn registry_kills_one_job_by_id() {
        let dir = std::env::temp_dir();
        let run = spawn_shell("sleep 30", &dir, None, None).unwrap();
        let done = run.done.clone();
        let reg = Registry::default();
        reg.shells.lock().unwrap().insert("bash_1".into(), Arc::new(Background { run, read: Mutex::new(0) }));
        assert!(!reg.kill("bash_2"));
        assert!(reg.kill("bash_1"));
        let code = tokio::time::timeout(Duration::from_secs(3), wait_done(done)).await.expect("stopped");
        assert_eq!(code, -1);
    }

    #[tokio::test]
    async fn session_stop_waits_for_background_group_members() {
        let dir = std::env::temp_dir().join(format!("pb-shell-group-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("late-marker");
        let command = format!("(sleep 1; touch {}) & wait", marker.display());
        let run = spawn_shell(&command, &dir, None, None).unwrap();
        let registry = Registry::default();
        registry.shells.lock().unwrap().insert(
            "bash_group".into(),
            Arc::new(Background {
                run,
                read: Mutex::new(0),
            }),
        );
        tokio::time::timeout(Duration::from_secs(3), registry.kill_all())
            .await
            .expect("session stop drains the shell group")
            .expect("group stop succeeds");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            !marker.exists(),
            "background group member survived session stop"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn eperm_with_live_shell_member_fails_within_deadline() {
        let started = tokio::time::Instant::now();
        let result = resolve_shell_signal_error(
            std::io::Error::from_raw_os_error(libc::EPERM),
            || async { Ok(true) },
        ).await;
        assert!(result.is_err(), "live member plus EPERM must fail closed");
        assert!(started.elapsed() < GROUP_STOP_TIMEOUT, "EPERM probe must not hang shutdown");
    }

    #[tokio::test]
    async fn shell_group_probe_error_fails_within_deadline() {
        let started = tokio::time::Instant::now();
        let result = resolve_shell_signal_error(
            std::io::Error::from_raw_os_error(libc::EPERM),
            || async { Err(anyhow!("injected process probe failure")) },
        ).await;
        assert!(result.unwrap_err().to_string().contains("injected process probe failure"));
        assert!(started.elapsed() < GROUP_STOP_TIMEOUT, "probe failure must not hang shutdown");
    }

    #[tokio::test]
    async fn zombie_only_shell_group_eperm_is_gone() {
        let result = resolve_shell_signal_error(
            std::io::Error::from_raw_os_error(libc::EPERM),
            || async { Ok(false) },
        ).await;
        assert!(result.is_ok(), "EPERM with no live group member means gone");
    }

    #[tokio::test]
    async fn shell_group_exit_deadline_is_bounded() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(40);
        let started = tokio::time::Instant::now();
        let result = wait_shell_group_exit_with(deadline, || async { Ok(true) }).await;
        assert!(result.unwrap_err().to_string().contains("termination deadline"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn failed_start_kill_and_non_exiting_child_returns_error_by_deadline() {
        let outer_deadline = Duration::from_millis(150);
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            outer_deadline,
            wait_for_child_exit_with(
                Err(anyhow!("injected start_kill failure")),
                std::future::pending::<std::io::Result<()>>(),
                Duration::from_millis(25),
            ),
        ).await;
        let inner = result.expect("child wait helper must return before the outer deadline");
        assert!(format!("{:#}", inner.unwrap_err()).contains("injected start_kill failure"));
        assert!(started.elapsed() < outer_deadline);
    }

    #[tokio::test]
    async fn kill_all_returns_error_when_a_job_never_reports_done() {
        let registry = Registry::default();
        let (done_tx, done_rx) = watch::channel(None);
        let run = Running {
            buf: Arc::new(Mutex::new(String::new())),
            pid: None,
            done: done_rx,
            kill: CancellationToken::new(),
        };
        registry.shells.lock().unwrap().insert(
            "stuck".into(),
            Arc::new(Background { run, read: Mutex::new(0) }),
        );
        let outer_deadline = Duration::from_millis(150);
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            outer_deadline,
            registry.kill_all_with_budgets(Duration::from_secs(1), Duration::from_millis(25)),
        ).await;
        let inner = result.expect("kill_all must return before the outer deadline");
        assert!(inner.unwrap_err().to_string().contains("did not report stopped"));
        assert!(started.elapsed() < outer_deadline);
        drop(done_tx);
    }

    #[tokio::test]
    async fn kill_all_surfaces_a_job_stop_failure() {
        let (done_tx, done_rx) = watch::channel(None);
        done_tx.send(Some(-2)).unwrap();
        let result = wait_done_receivers_with(vec![done_rx], Duration::from_millis(25)).await;
        assert!(result.unwrap_err().to_string().contains("refusing session reuse"));
    }

    #[tokio::test]
    async fn cd_carries_over_through_the_cwd_file() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("pb-cwd-test-{}", uuid::Uuid::new_v4()));
        let mut run = spawn_shell("cd /", &dir, Some(file.clone()), None).unwrap();
        finished(&mut run).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "/");
        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn the_shell_gets_the_extended_path() {
        let dir = std::env::temp_dir();
        let mut run = spawn_shell("printf %s \"$PATH\"", &dir, None, None).unwrap();
        assert_eq!(finished(&mut run).await, 0);
        let path = run.output();
        assert_eq!(Some(path.trim()), crate::proc::child_path(None).as_deref(), "the shell's PATH");
        assert!(path.split(':').any(|d| d == "/opt/homebrew/bin"), "{path}");
    }
}
