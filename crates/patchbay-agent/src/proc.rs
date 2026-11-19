//! Child processes: no console window of their own, and a PATH that finds the user's tools.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Keeps a spawned child from getting a console window.
///
/// The desktop app is a GUI-subsystem process with no console, so Windows gives every console
/// program it starts (npm, node, git, a shell) a fresh visible one, and for a long-lived child
/// that window and its taskbar entry stay until the child exits. Everything here reads pipes or
/// output, so the console is of no use. Does nothing off Windows.
pub trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl NoWindow for std::process::Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

impl NoWindow for tokio::process::Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

/// PATH plus the user-level bin directories a desktop launch may leave out of it.
///
/// An app started from the Dock, Finder or a desktop entry gets the bare launchd/session PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin` on macOS), so tools installed by Homebrew, npm, cargo and
/// the like would not be found by name.
pub fn search_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    search_dirs_from(std::env::var_os("PATH"), home)
}

fn search_dirs_from(path: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = home {
        for d in [".local/bin", "bin", ".npm-global/bin", ".bun/bin", ".opencode/bin", ".cursor/bin", ".volta/bin", ".cargo/bin"] {
            dirs.push(home.join(d));
        }
    }
    if cfg!(unix) {
        dirs.push("/usr/local/bin".into());
        dirs.push("/opt/homebrew/bin".into());
    }
    dirs
}

/// PATH for a child process: `first` (when given), then [`search_dirs`], each directory once.
pub fn child_path(first: Option<&Path>) -> Option<String> {
    join_dirs(first, search_dirs())
}

// --- the platform shell's command line ---

/// How one argument reaches the platform shell's command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptArg {
    /// A normal argv element. Windows MSVC-escapes it, turning every inner `"` into `\"`.
    Plain(String),
    /// Text appended verbatim: the Windows `cmd /C` script, see [`script_args`].
    Raw(String),
}

/// The arguments that run `script` after `flag` (`/C` for cmd, `-c` for sh).
///
/// `cmd /C` reads the tail of its command line with cmd's own quote rules, not the C runtime's,
/// so a script passed as an ordinary argument arrives MSVC-escaped — every `"` becomes `\"`,
/// which cmd does not un-escape — and a hook or shell command quoting a path then fails
/// (pb-20261111-10: the exe is "not recognized", quotes end up inside the path). On Windows a
/// script that contains a quote is therefore wrapped in one quote pair and appended raw, the
/// pattern `CommandExt::raw_arg` documents for cmd.exe: with three or more quotes on the line
/// cmd's preserve-the-quotes rule cannot apply, so it strips exactly that outer pair and the
/// script reaches cmd byte-identical. Scripts without a quote take the ordinary path — the same
/// bytes cmd received before this fix — and every script does on other platforms, where the
/// argument goes to `execvp` with no escaping to distort it.
pub fn script_args(flag: &str, script: &str, windows: bool) -> Vec<ScriptArg> {
    if windows && script.contains('"') {
        vec![ScriptArg::Plain(flag.to_owned()), ScriptArg::Raw(format!("\"{script}\""))]
    } else {
        vec![ScriptArg::Plain(flag.to_owned()), ScriptArg::Plain(script.to_owned())]
    }
}

/// Put [`script_args`] for `flag` and `script` on `cmd` (the shell chosen by the caller).
pub fn apply_script_args<'a>(cmd: &'a mut tokio::process::Command, flag: &str, script: &str) -> &'a mut tokio::process::Command {
    for arg in script_args(flag, script, cfg!(windows)) {
        match arg {
            ScriptArg::Plain(s) => {
                cmd.arg(s);
            }
            ScriptArg::Raw(s) => {
                #[cfg(windows)]
                cmd.raw_arg(s);
                // Unreachable: script_args returns Raw only when its windows argument is true.
                #[cfg(not(windows))]
                cmd.arg(s);
            }
        }
    }
    cmd
}

fn join_dirs(first: Option<&Path>, rest: Vec<PathBuf>) -> Option<String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = first.filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(dir.to_path_buf());
    }
    for d in rest {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).ok().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_bare_launchd_path_gets_the_user_bin_dirs() {
        let dirs = search_dirs_from(Some("/usr/bin:/bin:/usr/sbin:/sbin".into()), Some("/Users/u".into()));
        let path = join_dirs(None, dirs).unwrap();
        let parts: Vec<&str> = path.split(':').collect();
        assert_eq!(&parts[..4], ["/usr/bin", "/bin", "/usr/sbin", "/sbin"], "the inherited PATH comes first: {path}");
        for want in ["/Users/u/.local/bin", "/Users/u/bin", "/Users/u/.cargo/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            assert!(parts.contains(&want), "{want} missing from {path}");
        }
    }

    #[test]
    fn first_leads_and_each_dir_appears_once() {
        let dirs = search_dirs_from(Some("/opt/homebrew/bin:/usr/bin".into()), None);
        let path = join_dirs(Some(Path::new("/tools/node/bin")), dirs).unwrap();
        assert_eq!(path, "/tools/node/bin:/opt/homebrew/bin:/usr/bin:/usr/local/bin");
    }
}

/// The shell command line, on any OS: these tests compute what Windows would put on it.
#[cfg(test)]
mod script_arg_tests {
    use super::*;

    /// What Windows actually writes for these arguments, mirroring std (`make_command_line` in
    /// std's `sys/process/windows.rs` and `append_arg` in `sys/args/windows.rs`): the program
    /// always quoted, one space per argument, ordinary arguments MSVC-escaped, raw arguments
    /// appended verbatim.
    fn windows_command_line(program: &str, args: &[ScriptArg]) -> String {
        let mut line = format!("\"{program}\"");
        for arg in args {
            line.push(' ');
            match arg {
                ScriptArg::Raw(s) => line.push_str(s),
                ScriptArg::Plain(s) => line.push_str(&msvc_escape(s)),
            }
        }
        line
    }

    /// std's escaping of an ordinary argument: quoted when it holds a space or tab (or is
    /// empty), `"` written as `\"`, a backslash doubled only when a quote follows it.
    fn msvc_escape(arg: &str) -> String {
        let quoted = arg.contains(' ') || arg.contains('\t') || arg.is_empty();
        let mut out = String::new();
        if quoted {
            out.push('"');
        }
        let mut backslashes = 0;
        for c in arg.chars() {
            if c == '\\' {
                backslashes += 1;
            } else {
                if c == '"' {
                    out.push_str(&"\\".repeat(backslashes + 1));
                }
                backslashes = 0;
            }
            out.push(c);
        }
        if quoted {
            out.push_str(&"\\".repeat(backslashes));
            out.push('"');
        }
        out
    }

    #[test]
    fn a_quoted_path_reaches_cmd_unmangled() {
        let script = r#"run "C:\Program Files\tool\bin.exe" --flag"#;
        let args = script_args("/C", script, true);
        let line = windows_command_line("cmd", &args);
        assert_eq!(line, r#""cmd" /C "run "C:\Program Files\tool\bin.exe" --flag""#);
        assert!(!line.contains("\\\""), "no MSVC-escaped quote may reach cmd /C: {line}");
        // The bytes cmd used to get, for contrast: this is the defect the Raw path replaces.
        let old = windows_command_line("cmd", &[ScriptArg::Plain("/C".into()), ScriptArg::Plain(script.into())]);
        assert_eq!(old, r#""cmd" /C "run \"C:\Program Files\tool\bin.exe\" --flag""#);
    }

    #[test]
    fn plain_commands_are_byte_identical_to_the_old_path() {
        for (script, want) in [
            ("git status", r#""cmd" /C "git status""#),
            ("echo", r#""cmd" /C echo"#),
        ] {
            let args = script_args("/C", script, true);
            assert_eq!(args, [ScriptArg::Plain("/C".into()), ScriptArg::Plain(script.into())], "{script}");
            assert_eq!(windows_command_line("cmd", &args), want, "{script}");
        }
    }

    #[test]
    fn other_platforms_always_get_plain_arguments() {
        let script = r#"run "C:\Program Files\tool\bin.exe""#;
        assert_eq!(script_args("-c", script, false), [ScriptArg::Plain("-c".into()), ScriptArg::Plain(script.into())]);
        assert_eq!(script_args("/C", "git status", false), [ScriptArg::Plain("/C".into()), ScriptArg::Plain("git status".into())]);
    }

    /// The end-to-end proof on Windows (runs on CI's windows job): `dir /b "<path>"` lists the
    /// file only if cmd received the quotes as quotes — with the old MSVC-escaped bytes cmd
    /// gets `\` + path and prints "File Not Found".
    #[cfg(windows)]
    #[tokio::test]
    async fn a_quoted_path_runs_through_cmd() {
        let dir = std::env::temp_dir().join(format!("pb-cmd-quote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "").unwrap();
        let script = format!("dir /b \"{}\"", dir.display());
        let mut cmd = tokio::process::Command::new("cmd");
        apply_script_args(&mut cmd, "/C", &script);
        let out = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("marker.txt"), "cmd must see the quoted path un-mangled: {stdout} / {:?}", out.stderr);
        let _ = std::fs::remove_dir_all(dir);
    }
}
