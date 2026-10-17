# Developing Patchbay

Notes for working on Patchbay itself. For what the app does and how to install it, see the [README](../README.md).

## How it reuses Grok Build

[Grok Build](https://github.com/xai-org/grok-build) (Apache-2.0) is vendored as a git submodule at `vendor/grok-build`, pinned to a fixed commit. Local fixes live in `patches/` and are applied idempotently by `scripts/apply-patches.sh`, which `build-sidecar.sh` runs. Keep this set small.

- `0001-chat-completions-omit-model_id.patch`: stops sending xAI's non-standard `model_id` on Chat Completions messages, which strict endpoints such as Fireworks reject with a 400.
- `0002-chat-completions-accept-null-numbers.patch` treats explicit `null` token counts and indexes in Chat Completions responses and stream chunks as `0`. Fireworks sends these, and the unpatched parser fails with "invalid type: null, expected u32".
- `0003-messages-pdf-document-blocks.patch`: the Anthropic Messages encoder turns a `data:application/pdf` part into a `document` block, so PDFs can be sent natively.
- `0004-claude-plugins-env-gate.patch`: `GROK_CLAUDE_PLUGINS_ENABLED=false` stops the agent from loading Claude Code plugins. Grok's `[compat.claude]` config has no switch for plugins.
- `0005-windows-find-msvc-tools.patch` bumps `find-msvc-tools` to 0.1.14 in Grok Build's `Cargo.lock`. With 0.1.13, `cc` does not compile on Windows.
- `0006-windows-proto-build-deps.patch`: on Windows, Grok Build's proto build helper watches the proto files directly instead of asking `protoc` to write a dependency list to `/dev/stdout`, which Windows lacks.

| Patchbay piece | Grok Build plumbing |
| --- | --- |
| Chat streaming for all three wire protocols | `xai-grok-sampler` + `xai-grok-sampling-types`, linked in-process |
| Code agent: tools, permissions, sessions, compaction | the `grok` binary run as a sidecar: `grok agent --no-leader stdio` speaking [ACP](https://agentclientprotocol.com) |
| ACP client | `agent-client-protocol` 0.10.4, the same version Grok Build uses |

### How the Code agent is configured

Patchbay gives the sidecar a private `GROK_HOME` at `<app data>/grok`. It generates `config.toml` there from your Patchbay providers.

API keys never touch that file. Each key goes to the child process as an environment variable, which the config references with `env_key` (Bearer auth) or `env_http_headers` (`x-api-key`).

The generated config also does the following:

- It turns off telemetry, remote catalog fetches, auto-update and the leader daemon.
- It limits the model picker to Patchbay-defined models (`pb-*`).
- It writes `[compat.claude]` from Settings → Code agent → "From your Claude Code setup". By default the agent also loads your Claude Code skills, plugins, CLAUDE.md, subagents, MCP servers and hooks from `~/.claude` and project `.claude/` folders; each can be turned off there.

## Requirements

- Rust (stable; tested with 1.99) and `protoc` on `PATH`. DotSlash is not needed.
- Node 20+ and pnpm.
- The Tauri 2 system dependencies. On Linux this means webkit2gtk-4.1 (see the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)).

## Build and run

```bash
git submodule update --init
scripts/apply-patches.sh
pnpm install
scripts/build-sidecar.sh      # builds vendor/grok-build -> src-tauri/binaries/grok-<triple>
pnpm tauri dev
```

The first sidecar build compiles about 800 crates and takes a few minutes. For a release bundle, run `PROFILE=release scripts/build-sidecar.sh`, then `pnpm tauri build`.

Open **Settings** to add a provider. Presets are available for OpenAI, Anthropic, OpenRouter, Ollama and LM Studio. Then fetch or type model ids, and pick a default model.

## Testing without a real provider

`scripts/mock-llm.py` is a mock server that speaks OpenAI Chat Completions and Anthropic Messages. It rejects non-standard per-message fields as strictly as Fireworks does, and echoes back what the model received: the text, the number of images and the names of attached files.

When the request includes tools, the mock also triggers tool calls. Put `!run` in the prompt for a shell call, or `!edit` for a file edit. `!tools` lists the tool names it was offered.

`scripts/mock-mcp.py` is a minimal stdio MCP server with one `pb_echo` tool. It is used by the MCP smoke test.

```bash
scripts/mock-llm.py --port 8765 &
cd src-tauri
cargo test --lib                              # unit tests
cargo test --lib smoke -- --ignored           # chat (both backends), title generation, sidecar config, full ACP agent flow + resume
PATCHBAY_SMOKE_KIND=openai cargo test --lib smoke_code -- --ignored
```

To try the UI alone in a browser, run `pnpm dev` and open http://localhost:1420. Outside Tauri, a dev-only in-memory fake backend (`src/dev/fakeBackend.ts`) stands in for Rust. Add `?mesh=1` to the URL to start as a member of a fake four-computer mesh, with a pending join request, sessions running on other computers (one of them offline) and a chat reply streaming in from another computer.

The mesh's end-to-end tests run real nodes over WebSocket on localhost: `cargo test --lib mesh`.

## Layout

```
src-tauri/src/
  lib.rs            Tauri commands + state
  db.rs             SQLite schema (providers, models, conversations, messages, attachments, code sessions)
  providers.rs      keyring secrets, /models discovery, SamplerConfig mapping
  chat.rs           Chat streaming over xai-grok-sampler
  attachments.rs    ingest/sniff/store; images downscaled, PDFs text-extracted (pdf_oxide) with page-render fallback
  code/grok_config.rs  sidecar config.toml rendering (Grok-specific)
  code/acp.rs       ACP client (one sidecar per code session, LocalSet thread)
  code/mcp.rs       MCP server settings -> ACP `mcpServers` (secrets in the keyring)
  code/permissions.rs  "always allow" rules matched against ACP tool calls
  desktop.rs        tray icon, quick-entry window and its global shortcut
  files.rs          project file listing for @-mentions (honors .gitignore)
  terminal.rs       PTY shells (portable-pty)
src/                React + TypeScript UI (Vite, Tailwind, zustand, react-markdown + shiki, xterm.js)
scripts/            build-sidecar.sh, mock-llm.py, mock-mcp.py
```

## Known limitations (technical)

- PDFs reach non-Anthropic models as extracted text. Pages without extractable text are sent as images, up to 20 pages.
- The sampler adds `x-grok-*` request headers and a `grok-shell` User-Agent. Most endpoints ignore them.
- Chat has no tools yet, so MCP servers apply to the Code tab only.
- Global shortcuts may not reach apps on Wayland desktops. The tray icon needs a StatusNotifier host on Linux.
- Office formats (docx/xlsx/pptx), packaging/signing and auto-update are out of scope for this POC.

## Branches and releases

- `staging` is the default branch. Work goes on a feature branch (`feat/…`, `fix/…`, `chore/…`) and merges into `staging` by pull request.
- `release` only takes pull requests from `staging`. Merge them with **Create a merge commit**.
- Pull requests into either branch run `checks.yml`: static checks (`scripts/ci/check.sh`, actionlint), the frontend build, the third-party notices check, and the Rust unit tests.
- Pull requests into `release` also run `build.yml`, which builds every package: `.deb`, `.rpm` and AppImage, an Arch package, and best-effort Windows (NSIS, MSI) and macOS (DMG) builds. Run it by hand with workflow_dispatch to test the build on any branch.
- Merging into `release` runs `release.yml`. It reuses the pull request's build artifacts, signs each one with the release key, builds a signed pacman repository, tags `v<version>` and creates the GitHub Release. Windows and macOS packages ship only if they built.
- Every release needs a new version. `scripts/ci/version.sh` checks that `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json` and the PKGBUILD agree.

The signing key's public half is `packaging/patchbay-signing-key.asc`. Its private half is the `PATCHBAY_GPG_KEY` repository secret.

## License and third-party notices

Patchbay is Apache-2.0 (`LICENSE`, `NOTICE`). After changing Rust or npm dependencies, run `pnpm notices` and commit the updated `THIRD_PARTY_NOTICES.md`; CI fails when it is stale. Each file in `patches/` starts with a header naming the Grok Build files it changes, as the Apache license requires for modified files.
