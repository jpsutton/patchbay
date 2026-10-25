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
    patchbay_lib::run()
}
