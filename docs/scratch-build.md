# Scratch build: run a Patchbay beside the live one

A scratch build is the desktop app with its own identity, so testing it cannot touch the data of
the Patchbay you use. Tauri derives the data folder, the WebKit store and the Launch Services
identity from `identifier`; `src-tauri/tauri.scratch.conf.json` overrides it at build time and
needs no product code for that part.

```sh
pnpm tauri build --debug --bundles app --config src-tauri/tauri.scratch.conf.json
# -> src-tauri/target/debug/bundle/macos/Patchbay Scratch.app
python3 scripts/scratch_rig.py run "src-tauri/target/debug/bundle/macos/Patchbay Scratch.app"
```

What the overlay sets: `identifier` `dev.patchbay.scratch`, `productName` `Patchbay Scratch`, and
`plugins.updater.endpoints` `[]` so it can never update itself into a real release.

What the product code does for a non-default data folder (the app's default is
`default_data_dir()`, which keeps the real identifier on purpose so a daemon can take over):

- Provider, MCP and web-search keychain accounts get a `#<scope>` suffix, as the mesh secrets
  already did (`providers::set_keyring_scope`, called from `open_locked_state`).
- The artifact share server takes a port of its own (47000-47999, derived from the folder) instead
  of 4420, so it cannot take the port from the live app if that one is down.

Rules for running one:

- Launch it by full path, never from `/Applications`; `scratch_rig.py` runs `lsregister -u` at
  teardown so URL-scheme handling cannot be hijacked.
- Use only the native `patchbay` agent with a scratch-scoped provider key. Claude, Codex and Cursor
  sessions read the real accounts through `HOME`.
- `scratch_rig.py` reads metadata only: sizes and mtimes of the live data folder and hashed
  account names of the unscoped keychain entries, never a value. The live app writes its own
  database while it runs, so mtimes are reported, not gated; the gates are that the scratch process
  holds no file under the live folder, the unscoped keychain lines are unchanged, and teardown
  leaves nothing named `dev.patchbay.scratch`.
- `cargo test --lib -- --ignored scoped_keychain` writes one uniquely named probe entry to the
  real keychain and deletes it; it proves the scoped account is the only one written.
