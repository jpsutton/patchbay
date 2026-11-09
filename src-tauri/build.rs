fn main() {
    if std::env::var("TARGET").unwrap_or_default().contains("windows") {
        suppress_console_window_in_portable_pty();
    }
    #[cfg(feature = "desktop")]
    tauri_build::build();
}

/// Patch the unpacked portable-pty source (used for the embedded terminal) so that
/// ConPTY-spawned children also receive CREATE_NO_WINDOW. Without it a visible
/// console host window appears on Windows even though the parent is a GUI app.
/// We only ever need this during windows-target builds; the source is the same.
fn suppress_console_window_in_portable_pty() {
    let candidates = [
        std::env::var("CARGO_HOME").ok().map(|h| format!("{h}/registry/src")),
        std::env::var("HOME").ok().map(|h| format!("{h}/.cargo/registry/src")),
        std::env::var("USERPROFILE").ok().map(|h| format!("{h}\\.cargo\\registry\\src")),
    ];
    for base in candidates.into_iter().flatten() {
        let reg = std::path::Path::new(&base);
        if !reg.exists() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(reg) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("index.crates.io") {
                    let p = e.path().join("portable-pty-0.9.0/src/win/psuedocon.rs");
                    if p.exists() {
                        if let Ok(mut s) = std::fs::read_to_string(&p) {
                            if s.contains("EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT")
                                && !s.contains("0x08000000")
                            {
                                s = s.replace(
                                    "EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT",
                                    "EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | 0x08000000",
                                );
                                let _ = std::fs::write(&p, s);
                            }
                        }
                        return;
                    }
                }
            }
        }
    }
}

