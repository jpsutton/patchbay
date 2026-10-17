// The plugin has no commands of its own for the web view; the app's Rust code calls it.
const COMMANDS: &[&str] = &[];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).android_path("android").build();
}
