import fs from "node:fs";
import path from "node:path";

// The real desktop host reports env!("CARGO_PKG_VERSION") (src-tauri/src/gateway.rs), which Cargo
// takes from the [package] version in src-tauri/Cargo.toml. Read that same value once, at build
// time, so the dev fake backend reports the real app version and can't drift from it again.
const cargo = fs.readFileSync(path.resolve(process.cwd(), "src-tauri/Cargo.toml"), "utf8");
const version = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version) throw new Error("Could not read [package] version from src-tauri/Cargo.toml");

// Shared by both Vite configs: the fake backend is loaded by the desktop dev preview
// (vite.config.ts) and the remote dev preview (vite.remote.config.ts), so both must define it.
export const appDefines = { __PATCHBAY_VERSION__: JSON.stringify(version) };
