# patchbayd: Patchbay without a window

`patchbayd` runs Patchbay on a computer with no display, such as a home server, a VM or a box in a closet. It does everything the desktop app does behind its window: chats, Code sessions with their agents and terminals, and the mesh. You use it from a browser, which gets the full Patchbay UI, or from the phone app and other computers in your mesh.

## Install

Download `patchbayd-<version>-linux-x86_64.tar.gz` from the [Releases page](https://github.com/jpsutton/patchbay/releases/latest). It's signed like every release file (see "Checking a download" in the main README). Unpack it into `~/.local/opt/patchbayd`:

```bash
mkdir -p ~/.local/opt/patchbayd
tar -xzf patchbayd-*-linux-x86_64.tar.gz -C ~/.local/opt/patchbayd --strip-components=1
```

The folder holds `bin/patchbayd`, `bin/grok` (the Grok Build agent) and `share/patchbay/web` (the UI it serves). Keep them together: the daemon finds the other two relative to its own binary. Other coding agents (Claude Code, OpenCode and so on) are found on `PATH` as in the desktop app.

## Run

```bash
~/.local/opt/patchbayd/bin/patchbayd
```

It prints the address of the web UI with its access token:

```
Patchbay web UI: http://127.0.0.1:4420/#token=…
```

Open that address in a browser. The token is saved in the browser, so after the first visit `http://127.0.0.1:4420/` is enough. Run `patchbayd token` to print the address again, or `patchbayd token --rotate` to replace the token, which signs out every browser.

To keep it running, install the systemd user unit:

```bash
mkdir -p ~/.config/systemd/user
cp ~/.local/opt/patchbayd/share/patchbay/patchbayd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now patchbayd
loginctl enable-linger "$USER"   # keeps it running while you're logged out
```

## Reaching it from another computer

By default the web UI listens on `127.0.0.1` only. To use it from your laptop, forward the port over SSH:

```bash
ssh -N -L 4420:127.0.0.1:4420 you@server
```

Then open `http://127.0.0.1:4420/` on the laptop.

`--listen 0.0.0.0:4420` serves it on the network instead, but only over plain HTTP. **Anyone who gets the token can run commands on the server as you**, through the agents and the terminal. Prefer the SSH tunnel, or put the daemon behind a reverse proxy that adds HTTPS.

## Options

| Option | Meaning |
| --- | --- |
| `--data-dir <dir>` | Data folder. Default: the desktop app's (`~/.local/share/dev.patchbay.app`), so a computer that ran the app keeps its chats, sessions and mesh membership. |
| `--listen <addr>` | Web UI address. Default `127.0.0.1:4420`. |
| `--ui-dir <dir>` | The built UI to serve, if it isn't in the usual place. |
| `--no-web` | Don't serve the web UI. The node is then reachable through the mesh only. |
| `PATCHBAY_LOG` | Log filter, e.g. `debug` or `patchbay_lib=debug`. Default `info`. |

Only one Patchbay may use a data folder at a time. `patchbayd` refuses to start while the desktop app runs on the same folder, and the app refuses while the daemon runs.

## What's different from the desktop app

- **Files come from the browser.** Attaching a file uploads it from the computer running the browser. Exports download through the browser.
- **Folders are on the server.** "Open folder" for a Code session browses folders under the server's home folder.
- **No desktop extras.** There's no tray icon, quick-entry window or global shortcut. Window controls and "Show in file manager" are hidden.
- **Secrets.** API keys go to the OS keyring when one runs. Servers usually have none, so keys are stored in the database inside the data folder. The daemon creates that folder readable by you only. If the folder already existed, check its permissions yourself.
- **Reconnecting.** If the daemon restarts, open tabs show a banner and reload when it's back. Chats and agent turns keep running on the daemon while no tab is open.

## Joining a mesh

Use **Settings → Mesh** in the web UI to create a mesh, or to join one with another member's address and the mesh passphrase, the same as in the desktop app. Once joined, phones can use the daemon as their gateway, and the desktop app can move Code sessions to and from it.

## Building it yourself

```bash
pnpm install && pnpm build
PROFILE=release scripts/build-sidecar.sh
scripts/package-daemon.sh       # -> src-tauri/target/release/bundle/daemon/patchbayd-*.tar.gz
```

For development, `cargo build --bin patchbayd --no-default-features --features web` in `src-tauri` builds it without Tauri, so a server doesn't need WebKitGTK or GTK. Without `--features web` you get a daemon with no browser UI.
