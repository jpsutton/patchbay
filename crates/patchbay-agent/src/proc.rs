//! Child processes that never need a console of their own.

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
