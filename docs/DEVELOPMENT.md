# Developing Patchbay

Notes for working on Patchbay itself. For what the app does and how to install it, see the [README](../README.md).

## How the Code agent and the LLM client are built

Patchbay owns both. Two crates in `crates/` do the work, and the app wires them in:

| Piece | Where |
| --- | --- |
| LLM client: one conversation model over OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Gemini; streaming, retries, tool calls, reasoning, prompt caching | `crates/patchbay-llm` |
| Coding agent: an ACP server with the prompt, tools, permissions, MCP client, plan mode, compaction, rewind and session store | `crates/patchbay-agent` |
| The app's side of that agent: providers, keys and settings for it | `src-tauri/src/code/native.rs` |
| ACP client for every agent, built in or external | `src-tauri/src/code/acp.rs`, on `agent-client-protocol` 0.10.4 |

The built-in agent runs inside the app and talks ACP to `acp.rs` over an in-memory pipe, so permissions, MCP settings, mesh relay, usage and the whole Code UI treat it like any other agent. `docs/design/harness.md` describes its design. External agents (Claude Code, OpenCode, Cursor CLI, Kilo Code, Codex, Grok Build) are spawned as child processes; `docs/design/agents.md` covers them.

### Grok Build

Grok Build is no longer part of the build. If `grok` is on `PATH` (or set in Settings → Coding agents), it runs as an external agent like the others. For that, Patchbay gives it a private `GROK_HOME` at `<app data>/grok` and generates `config.toml` there from your Patchbay providers (`code/grok_config.rs`). API keys never touch that file: each key goes to the child process as an environment variable, which the config references with `env_key` (Bearer auth) or `env_http_headers` (`x-api-key`). The generated config also turns off telemetry, remote catalog fetches, auto-update and the leader daemon, limits the model picker to Patchbay-defined models (`pb-*`), and writes `[compat.claude]` from Settings → "From your Claude Code setup".

## Requirements

- Rust (stable; tested with 1.99).
- Node 20+ and pnpm.
- The Tauri 2 system dependencies. On Linux this means webkit2gtk-4.1 (see the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)).

## Build and run

```bash
pnpm install
pnpm tauri dev
```

For a release bundle, run `pnpm tauri build`.

Open **Settings** to add a provider. Presets are available for OpenAI, Anthropic, OpenRouter, Ollama and LM Studio. Then fetch or type model ids, and pick a default model.

### The headless daemon

`cargo build --bin patchbayd --no-default-features --features web` builds `patchbayd` without Tauri. Commands are plain functions in `lib.rs`, listed once in `api.rs`. The window reaches them through a custom Tauri invoke handler, and browsers through the daemon's WebSocket, where `src/lib/webBackend.ts` stands in for Tauri's IPC. Desktop-only commands (quick entry, writing exports) stay `#[tauri::command]`s in `desktop.rs`. Views check `isWeb()` (`src/lib/platform.ts`) for what a browser lacks: native dialogs, local file paths, window controls.

To try it, `pnpm build`, then `target/debug/patchbayd --data-dir /tmp/pb --listen 127.0.0.1:4477` from `src-tauri`, and open the printed address. Use a scratch `--data-dir`: only one Patchbay may use a data folder at a time.

## Testing without a real provider

`scripts/mock-llm.py` is a mock server that speaks OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Gemini. It rejects non-standard per-message fields as strictly as Fireworks does, and echoes back what the model received: the text, the number of images and the names of attached files.

When the request includes tools, the mock also triggers tool calls. Put `!run` in the prompt for a shell call, `!edit` for a file edit, `!ask` for a multiple-choice question, `!search` for a web search (it also serves Brave's API with the key `brave-test-key`), and `!task`, `!par`, `!bg`, `!fork`, `!named`, `!msg` and `!wt` for the subagent kinds. `!tools` lists the tool names it was offered.

`scripts/mock-mcp.py` is a minimal stdio MCP server with one `pb_echo` tool. It is used by the MCP smoke test.

```bash
scripts/mock-llm.py --port 8765 &
cd src-tauri
cargo test --lib                              # unit tests
cargo test --lib smoke -- --ignored           # chat, title generation, the built-in agent through the Code tab: edit, permission, rewind, MCP, resume
PATCHBAY_SMOKE_KIND=openai cargo test --lib smoke_code -- --ignored
(cd ../crates/patchbay-llm && cargo test -- --ignored)     # all four wire formats, tool round trips
(cd ../crates/patchbay-agent && cargo test -- --ignored)   # the agent over ACP: every wire format, plan mode, compaction, rewind, questions, subagents
```

To drive the built-in agent by hand over stdio, build `crates/patchbay-agent` and run `patchbay-agent` with `PATCHBAY_AGENT_MODELS` pointing at a JSON list of models (see `src/main.rs`).

The other coding agents (Claude Code, OpenCode, Cursor CLI, Kilo Code) have live tests that run against the copies installed on your machine, with your own sign-ins. Agents that aren't installed are skipped:

```bash
cargo test --lib agents_live -- --ignored --nocapture        # start, list models, switch model, resume
PATCHBAY_LIVE_PROMPT=opencode,claude-code cargo test --lib agents_live -- --ignored --nocapture   # also one tiny prompt each
```

To try the UI alone in a browser, run `pnpm dev` and open http://localhost:1420. Outside Tauri, a dev-only in-memory fake backend (`src/dev/fakeBackend.ts`) stands in for Rust. Add `?mesh=1` to the URL to start as a member of a fake four-computer mesh, with a pending join request, sessions running on other computers (one of them offline) and a chat reply streaming in from another computer.

The mesh's end-to-end tests run real nodes over WebSocket on localhost: `cargo test --lib mesh`.

## Layout

```
src-tauri/src/
  lib.rs            commands + state, shared by the app and the daemon
  api.rs            command dispatcher: name + JSON args, for Tauri IPC and the daemon's WebSocket
  web.rs            patchbayd's HTTP/WebSocket server for the browser UI (feature `web`)
  bin/patchbayd.rs  the headless daemon (docs/headless.md)
  db.rs             SQLite schema (providers, models, conversations, messages, attachments, code sessions)
  providers.rs      keyring secrets, /models discovery, mapping to patchbay-llm's Config
  chat.rs           Chat streaming over patchbay-llm
  attachments.rs    ingest/sniff/store; images downscaled, PDFs text-extracted (pdf_oxide) with page-render fallback
  code/agents.rs    coding agent backends: finding, launching and configuring each one
  code/native.rs    the host of the built-in agent (providers, keys, settings)
  code/grok_config.rs  config.toml rendering for an external Grok Build (Grok-specific)
  code/acp.rs       ACP client (one agent process per code session, LocalSet thread)
  code/mcp.rs       MCP server settings -> ACP `mcpServers` (secrets in the keyring)
  code/permissions.rs  "always allow" rules matched against ACP tool calls
  desktop.rs        Tauri entry point, tray icon, quick-entry window and its global shortcut (feature `desktop`)
  files.rs          project file listing for @-mentions (honors .gitignore)
  terminal.rs       PTY shells (portable-pty)
src/                React + TypeScript UI (Vite, Tailwind, zustand, react-markdown + shiki, xterm.js)
crates/            patchbay-llm (LLM client), patchbay-agent (coding agent), patchbay-mesh (mesh protocol)
scripts/            mock-llm.py, mock-mcp.py, ci/, package-daemon.sh
```

## Known limitations (technical)

- PDFs reach models other than Anthropic's and Gemini's as extracted text. Pages without extractable text are sent as images, up to 20 pages.
- Chat has no tools yet, so MCP servers apply to the Code tab only.
- Global shortcuts may not reach apps on Wayland desktops. The tray icon needs a StatusNotifier host on Linux.
- Office formats (docx/xlsx/pptx), packaging/signing and auto-update are out of scope for this POC.

## Branches and releases

- `staging` is the default branch. Work goes on a feature branch (`feat/…`, `fix/…`, `chore/…`) and merges into `staging` by pull request.
- `release` only takes pull requests from `staging`. Merge them with **Create a merge commit**.
- Pull requests into either branch run `checks.yml`: static checks (`scripts/ci/check.sh`, actionlint), the frontend build, the third-party notices check, the Rust unit tests and the end-to-end tests of the LLM client and the agent against the mock model.
- Pull requests into `release` also run `build.yml`, which builds every package: `.deb`, `.rpm` and AppImage, an Arch package, and best-effort Windows (NSIS, MSI) and macOS (DMG) builds. Run it by hand with workflow_dispatch to test the build on any branch.
- Merging into `release` runs `release.yml`. It reuses the pull request's build artifacts, signs each one with the release key, builds a signed pacman repository, tags `v<version>` and creates the GitHub Release. Windows and macOS packages ship only if they built.
- Every release needs a new version. `scripts/ci/version.sh` checks that `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json` and the PKGBUILD agree.

The signing key's public half is `packaging/patchbay-signing-key.asc`. Its private half is the `PATCHBAY_GPG_KEY` repository secret.

## License and third-party notices

Patchbay is Apache-2.0 (`LICENSE`, `NOTICE`). After changing Rust or npm dependencies, run `pnpm notices` and commit the updated `THIRD_PARTY_NOTICES.md`; CI fails when it is stale.
