//! Embedded terminal: one PTY-backed user shell per terminal id.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tauri::ipc::Channel;

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TermEvent {
    /// Base64-encoded output bytes.
    Data { data: String },
    Exit { code: Option<u32> },
}

struct Term {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

#[derive(Default)]
pub struct TerminalManager {
    terms: Mutex<HashMap<String, Term>>,
}

impl TerminalManager {
    pub fn open(
        &self,
        id: &str,
        cwd: &str,
        shell: Option<&str>,
        cols: u16,
        rows: u16,
        channel: Channel<TermEvent>,
    ) -> Result<()> {
        self.close(id);
        let pty = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = match shell.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sh) => CommandBuilder::new(sh),
            None => CommandBuilder::new_default_prog(),
        };
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let mut child = pty.slave.spawn_command(cmd)?;
        drop(pty.slave);
        let killer = child.clone_killer();
        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;

        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if channel
                            .send(TermEvent::Data {
                                data: B64.encode(&buf[..n]),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let code = child.wait().ok().map(|s| s.exit_code());
            let _ = channel.send(TermEvent::Exit { code });
        });

        self.terms.lock().unwrap().insert(
            id.to_owned(),
            Term {
                master: pty.master,
                writer,
                killer,
            },
        );
        Ok(())
    }

    pub fn write(&self, id: &str, data: &str) -> Result<()> {
        let mut terms = self.terms.lock().unwrap();
        let t = terms.get_mut(id).ok_or_else(|| anyhow!("terminal not open"))?;
        t.writer.write_all(data.as_bytes())?;
        t.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let terms = self.terms.lock().unwrap();
        let t = terms.get(id).ok_or_else(|| anyhow!("terminal not open"))?;
        t.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn close(&self, id: &str) {
        if let Some(mut t) = self.terms.lock().unwrap().remove(id) {
            let _ = t.killer.kill();
        }
    }

    pub fn close_all(&self) {
        for (_, mut t) in self.terms.lock().unwrap().drain() {
            let _ = t.killer.kill();
        }
    }
}
