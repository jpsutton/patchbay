# patchbayd: Patchbay without a window

`patchbayd` runs Patchbay on a computer with no display, such as a home server, a VM or a box in a closet. It does everything the desktop app does behind its window: chats, Code sessions with their agents and terminals, and the mesh. You use it from a browser, which gets the full Patchbay UI, or from the phone app and other computers in your mesh.

## Install

Download it from the [Releases page](https://github.com/jpsutton/patchbay/releases/latest). Every file is signed like the rest of the release (see "Checking a download" in the main README).

| Platform | Package |
| --- | --- |
| Debian, Ubuntu | `patchbayd_<version>_amd64.deb` (Ubuntu 24.04 or newer, Debian 13) |
| Debian 12 or newer, Ubuntu 24.04 (arm64) | `patchbayd_<version>_arm64.deb`, or `sudo apt install patchbayd` from the Patchbay apt repository (set it up as in the main README) |
| Fedora, openSUSE | `patchbayd-<version>-1.x86_64.rpm` |
| Arch Linux | `patchbayd` in the jpsutton pacman repository (set it up as in the main README, then `sudo pacman -Sy patchbayd`) |
| Any Linux | `patchbayd-<version>-linux-x86_64.tar.gz` (glibc 2.39 or newer) |
| Windows | `patchbayd-<version>-windows-x86_64.zip` |
| macOS | `patchbayd-<version>-macos-aarch64.tar.gz` (Apple silicon) |

The packages install `patchbayd` and the web UI under `/usr/lib/patchbayd`, with `patchbayd` on your `PATH` and two systemd units (see below). They don't conflict with the desktop app's packages. The coding agent is built in. Other coding agents (Claude Code, OpenCode, Grok Build and so on) are found on `PATH` as in the desktop app.

The archives hold the same pieces. Keep them together, because the daemon finds the UI relative to its own binary. On Linux, unpack the tarball into `~/.local/opt/patchbayd`, where its systemd unit expects it:

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

Pick one per user. Both use the same data folder, and only one of them may run at a time.

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

Only one Patchbay process may own a data folder at a time. `patchbayd` refuses to start while the desktop app runs on the same folder.

## The desktop app on a running daemon

If you start the desktop app while `patchbayd` runs on the same data folder, the app attaches to the daemon and uses it as its backend. You get the native window, tray icon, quick-entry shortcut and notifications, while chats, Code sessions, terminals and the mesh run in the daemon. Closing the app leaves the daemon and its agents running.

- They talk over a socket in the data folder (`patchbayd.sock`; a named pipe on Windows), readable by you only. Each side proves it knows the token in `ipc-token`, which the daemon writes on every start, so no other process can pose as either one. The socket works with `--no-web` and any `--listen` address.
- The app and the daemon must be the same version. If they differ, the app says so and doesn't start. Update one of them, or stop the daemon to use the app on its own.
- If the daemon stops or restarts, the app shows a banner and reconnects when the daemon is back. It doesn't take the data folder over, so a restarting daemon can start again.
- The quick-entry shortcut and title bar settings apply to the app's window as usual. The daemon saves them with the other preferences.
- A second copy of the app (not the daemon) still can't start on the same folder.

## What's different from the desktop app

- **Files come from the browser.** Attaching a file uploads it from the computer running the browser. Exports download through the browser.
- **Folders are on the server.** "New session" in Code browses folders under the server's home folder.
- **No desktop extras.** There's no tray icon, quick-entry window or global shortcut. Window controls and "Show in file manager" are hidden.
- **Secrets.** API keys go to the OS keyring when one runs. Servers usually have none, so keys are stored in the database inside the data folder. The daemon creates that folder readable by you only. If the folder already existed, check its permissions yourself.
- **Reconnecting.** If the daemon restarts, open tabs show a banner and reload when it's back. Chats and agent turns keep running on the daemon while no tab is open.

## Claude subscription on a server

Patchbay's agent and Chat can use a Claude subscription through a provider set to "Use Claude Code login". On a server with no `claude` login, run `claude setup-token` on any computer and set the token it prints as `CLAUDE_CODE_OAUTH_TOKEN` in the daemon's environment, for example in a root-owned `EnvironmentFile=` (mode 600) of its systemd unit. Patchbay then uses that token for providers with no Claude config directory, ahead of any `~/.claude` login, as Claude Code does. It never renews the token: when it expires or is revoked, make a new one. Providers that name a config directory keep using that directory's login.

## Grok subscription on a server

A provider set to "Use Grok login (subscription)" reuses the sign-in that `grok login` keeps in `<grok home>/auth.json`: `$GROK_HOME` in the daemon's environment when it is set, else `~/.grok`, or the provider's own "Grok home". On a server, sign in as the user the daemon runs as, with the device-code flow (no browser on the server):

```bash
grok login --device-auth                  # default home, ~/.grok
GROK_HOME=/srv/grok-team grok login --device-auth   # another account; name that folder as the provider's Grok home
```

Unlike Claude's `setup-token`, there is no long-lived token to copy. Don't copy one computer's `auth.json` to another either: Grok's refresh tokens rotate, and xAI revokes the newer token when an older one is used again, so two copies sign each other out. Sign in on each computer. The login refreshes itself (Patchbay and `grok` share the file under a lock), so it keeps working until it is revoked or `grok logout` removes it; then sign in again.

## Joining a mesh

Use **Settings → Mesh** in the web UI to create a mesh, or to join one with another member's address and the mesh passphrase, the same as in the desktop app. Once joined, phones can use the daemon as their gateway, and the desktop app can move Code sessions to and from it.

### From the command line

With the daemon running, `patchbayd mesh` does the same without a browser. It talks to the running daemon through its socket, so run it as the daemon's user (or pass the same `--data-dir`). The passphrase comes from `PATCHBAY_MESH_PASSPHRASE`, or from the first line of stdin with `--passphrase-stdin`, so it never shows up in `ps`.

```bash
export PATCHBAY_MESH_PASSPHRASE='the mesh passphrase'
patchbayd mesh join 100.64.0.7 --name linode1      # prints a code, then waits for approval
patchbayd mesh status                              # members, and requests waiting for approval
patchbayd mesh approve laptop                      # on a member: a waiting node's name, id or id prefix
```

Check that the code `join` prints matches the one the member shows, then approve it there, either in **Settings → Mesh** or with `patchbayd mesh approve`. The other commands are `mesh create --name <name>` (start a new mesh), `mesh reject <node>` and `mesh leave`.

## Building it yourself

```bash
pnpm install && pnpm build
PROFILE=release scripts/build-sidecar.sh
scripts/package-daemon.sh       # -> src-tauri/target/release/bundle/daemon/ (tar, deb, rpm on Linux)
```

For development, `cargo build --bin patchbayd --no-default-features --features web` in `src-tauri` builds it without Tauri, so a server doesn't need WebKitGTK or GTK. With `--features daemon` instead of `web` you get a daemon with no browser UI.
