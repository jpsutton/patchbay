# patchbayd: Patchbay without a window

`patchbayd` runs Patchbay on a computer with no display, such as a home server, a VM or a box in a closet. It does everything the desktop app does behind its window: chats, Code sessions with their agents and terminals, and the mesh. You use it from a browser, which gets the full Patchbay UI, or from the phone app and other computers in your mesh.

## Install

Download it from the [Releases page](https://github.com/jpsutton/patchbay/releases/latest). Every file is signed like the rest of the release (see "Checking a download" in the main README).

| Platform | Package |
| --- | --- |
| Debian, Ubuntu | `patchbayd_<version>_amd64.deb` (Ubuntu 24.04 or newer, Debian 13) |
| Fedora, openSUSE | `patchbayd-<version>-1.x86_64.rpm` |
| Arch Linux | `patchbayd` in the Patchbay package repository (set it up as in the main README, then `sudo pacman -Sy patchbayd`) |
| Any Linux | `patchbayd-<version>-linux-x86_64.tar.gz` (glibc 2.39 or newer) |
| Windows | `patchbayd-<version>-windows-x86_64.zip` |
| macOS | `patchbayd-<version>-macos-aarch64.tar.gz` (Apple silicon) |

The packages install `patchbayd`, its own copy of `grok` (the Grok Build agent) and the web UI under `/usr/lib/patchbayd`, with `patchbayd` on your `PATH` and two systemd units (see below). They don't conflict with the desktop app's packages. Other coding agents (Claude Code, OpenCode and so on) are found on `PATH` as in the desktop app.

The archives hold the same three pieces. Keep them together, because the daemon finds `grok` and the UI relative to its own binary. On Linux, unpack the tarball into `~/.local/opt/patchbayd`, where its systemd unit expects it:

```bash
mkdir -p ~/.local/opt/patchbayd
tar -xzf patchbayd-*-linux-x86_64.tar.gz -C ~/.local/opt/patchbayd --strip-components=1
```

## Run

```bash
patchbayd        # ~/.local/opt/patchbayd/bin/patchbayd from the tarball
```

It prints the address of the web UI with its access token:

```
Patchbay web UI: http://127.0.0.1:4420/#token=…
```

Open that address in a browser. The token is saved in the browser, so after the first visit `http://127.0.0.1:4420/` is enough. Run `patchbayd token` to print the address again, or `patchbayd token --rotate` to replace the token, which signs out every browser.

## Keep it running (systemd)

The Linux packages include two units:

- **`patchbayd.service`**, a user unit: Patchbay runs as you, with your data.

  ```bash
  systemctl --user enable --now patchbayd
  loginctl enable-linger "$USER"   # keeps it running while you're logged out
  ```

- **`patchbayd@.service`**, a system unit that starts at boot as the user named after the `@`, without anyone logging in:

  ```bash
  sudo systemctl enable --now patchbayd@alice
  ```

Pick one per user. Both use the same data folder, and only one Patchbay may use it at a time.

The tarball ships the user unit in `share/patchbay/patchbayd.service`, pointed at `~/.local/opt/patchbayd`:

```bash
mkdir -p ~/.config/systemd/user
cp ~/.local/opt/patchbayd/share/patchbay/patchbayd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now patchbayd
```

Logs go to the journal: `journalctl --user -u patchbayd`, or `journalctl -u patchbayd@alice`. On first start the address with the token is logged there too, and `patchbayd token` prints it any time.

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
- **Folders are on the server.** "New session" in Code browses folders under the server's home folder.
- **No desktop extras.** There's no tray icon, quick-entry window or global shortcut. Window controls and "Show in file manager" are hidden.
- **Secrets.** API keys go to the OS keyring when one runs. Servers usually have none, so keys are stored in the database inside the data folder. The daemon creates that folder readable by you only. If the folder already existed, check its permissions yourself.
- **Reconnecting.** If the daemon restarts, open tabs show a banner and reload when it's back. Chats and agent turns keep running on the daemon while no tab is open.

## Joining a mesh

Use **Settings → Mesh** in the web UI to create a mesh, or to join one with another member's address and the mesh passphrase, the same as in the desktop app. Once joined, phones can use the daemon as their gateway, and the desktop app can move Code sessions to and from it.

## Building it yourself

```bash
pnpm install && pnpm build
PROFILE=release scripts/build-sidecar.sh
scripts/package-daemon.sh       # -> src-tauri/target/release/bundle/daemon/ (tar, deb, rpm on Linux)
```

For development, `cargo build --bin patchbayd --no-default-features --features web` in `src-tauri` builds it without Tauri, so a server doesn't need WebKitGTK or GTK. With `--features daemon` instead of `web` you get a daemon with no browser UI.
