// Prevents an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--agent-messaging-mcp") {
        if let Err(e) = patchbay_lib::agent_messaging::run_stdio() {
            eprintln!("Patchbay agent messaging MCP: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Answer these before Tauri starts. Anything else would open a whole second app window (it
    // attaches to patchbayd when that holds the data folder), which is not what a script or an
    // agent asking for the version wants.
    match std::env::args().nth(1).as_deref() {
        Some("-V" | "--version") => {
            println!("patchbay {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("-h" | "--help") => {
            println!(
                "patchbay {}\n\nUsage: patchbay [-V | --version] [-h | --help]\n\nStarts the Patchbay desktop app. For the headless daemon, run patchbayd.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        _ => {}
    }
    patchbay_lib::run()
}
