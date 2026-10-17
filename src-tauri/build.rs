fn main() {
    // Used to locate the dev sidecar at binaries/grok-<target-triple>.
    println!(
        "cargo:rustc-env=PATCHBAY_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    tauri_build::build()
}
