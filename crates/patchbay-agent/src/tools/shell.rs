//! Bash, and the background shells BashOutput and KillShell manage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
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

/// A running command.
pub struct Running {
    buf: Arc<Mutex<String>>,
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
}

impl Registry {
    pub fn cwd(&self, fallback: &Path) -> PathBuf {
        self.cwd.lock().unwrap().clone().filter(|p| p.is_dir()).unwrap_or_else(|| fallback.to_path_buf())
    }

    /// Stop every background command (the session is closing).
    pub fn kill_all(&self) {
        for b in self.shells.lock().unwrap().values() {
            b.run.kill.cancel();
        }
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
    cmd.arg(flag)
        .arg(script)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .kill_on_drop(true);
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
                kill_tree(pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                -1
            }
        };
        // Output the process left behind in the pipes still counts.
        let _ = tokio::time::timeout(Duration::from_millis(500), futures::future::join_all(readers)).await;
        let _ = done_tx.send(Some(code));
    });
    Ok(Running { buf, done: done_rx, kill })
}

#[cfg(unix)]
fn kill_tree(pid: Option<u32>) {
    if let Some(pid) = pid {
        // The command leads its own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_tree(_pid: Option<u32>) {}

async fn finished(run: &mut Running) -> i32 {
    loop {
        if let Some(c) = *run.done.borrow() {
            return c;
        }
        if run.done.changed().await.is_err() {
            return -1;
        }
    }
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
Do not sleep in a loop to wait: read the output instead."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "description": {"type": "string", "description": "A short description of what it does"},
                    "timeout": {"type": "integer", "description": "Timeout in milliseconds (max 600000)"},
                    "run_in_background": {"type": "boolean", "description": "Run in the background and return an id"}
                },
                "required": ["command"]
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

    fn needs_permission(&self, _: &Value, _: &Session) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(command) = str_arg(&input, "command").filter(|c| !c.trim().is_empty()) else {
            return ToolOutput::error("command is required");
        };
        let session = &ctx.session;
        let dir = session.shells.cwd(&session.cwd);
        if bool_arg(&input, "run_in_background") {
            return start_background(session, command, &dir);
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
        };
        let code = match outcome {
            Ok(c) => c,
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

fn start_background(session: &Session, command: &str, dir: &Path) -> ToolOutput {
    let run = match spawn_shell(command, dir, None, None) {
        Ok(r) => r,
        Err(e) => return ToolOutput::error(e),
    };
    let id = format!("bash_{}", session.shells.next.fetch_add(1, Ordering::Relaxed) + 1);
    session
        .shells
        .shells
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::new(Background { run, read: Mutex::new(0) }));
    ToolOutput::text(format!(
        "Command running in background with ID: {id}. Read its output with BashOutput, stop it with KillShell."
    ))
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
expression: only matching lines come back."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "The id Bash returned"},
                    "filter": {"type": "string", "description": "Regular expression for the lines to keep"}
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
    async fn cd_carries_over_through_the_cwd_file() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("pb-cwd-test-{}", uuid::Uuid::new_v4()));
        let mut run = spawn_shell("cd /", &dir, Some(file.clone()), None).unwrap();
        finished(&mut run).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "/");
        let _ = std::fs::remove_file(file);
    }
}
