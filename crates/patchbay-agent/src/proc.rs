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
