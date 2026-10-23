# Patchbay Remote: design

A phone app that drives a mesh's chats and Code sessions. It does no work of its own: no agents, no LLM calls, no synced database. Android comes first. Everything outside the Android-specific parts is written so that an iOS client can follow without changes to the protocol. This builds on [mesh.md](mesh.md); read that first. For building and running the app, see [../../remote/README.md](../../remote/README.md).

The code: `crates/patchbay-mesh` (protocol and the phone's client), `src-tauri/src/mesh/node.rs` and `src-tauri/src/gateway.rs` (the desktop side), `remote/` (the phone app), `src/remote/` (its UI).

## Goals

1. **Thin.** The phone joins the mesh, then views and drives what other members run. It stores no provider keys and no copy of the database.
2. **Chats and Code sessions in v1.** List them, read them live, send, stop, and answer permission prompts.
3. **Fast resume.** Opening the app after a disconnect, a network change or hours in the background shows the current state in about one round trip. That holds whatever happened in between.
4. **Survives network changes.** Moving from home Wi-Fi (a member on the LAN) to cellular with ZeroTier switches gateways without losing anything.

## Non-goals

- Hosting Code sessions, running agents, or making LLM calls on the phone.
- Push notifications through Apple or Google services. They would put a cloud broker back in. See section 7.
- A custom UDP transport. See section 5.
- Terminals, and moving or taking over sessions, in v1.

## 1. Remote members

A phone joins with the same SPAKE2 and Noise handshake and the same six-digit approval as any node. Its admission record carries `role: "remote"`, signed with the rest of the record. A full member's record has no role. Its signature covers exactly the bytes it did before roles existed, so existing records stay valid.

A remote member:

- never receives the mesh data key or `mesh_secrets` (its `Admitted` message carries an empty key), so a lost phone leaks no provider keys;
- doesn't take part in database sync (`SyncVv`, `SyncOps`), link reports or relaying, and nobody dials it;
- isn't a host, a move or take-over target, or a checkpoint mirror;
- shows in Settings → Mesh as a phone, and is removed the same way as any member. Removing it rotates no key, since it never had one. Records don't sync to phones, so its gateway tells it with `Rejected { "this device was removed from the mesh" }`, and the phone forgets the mesh.

Remote control is remote code execution on every host, the same as for a desktop member. Admission stays the trust boundary. The phone can ask for its fingerprint, face or screen lock when the app opens and after five minutes away (`tauri-plugin-biometric`), because phones get lost more often than laptops.

**Compatibility.** An older desktop would ignore an unknown `role` and admit a phone as a full member. So desktops that serve phones say so with `features: ["remote-gateway"]` in their `Hello`, and the phone waits for the gateway's `Hello` before sending its own. A node without the feature never sees the phone's `Hello`, and the phone tells the user to update that computer. `PROTOCOL_VERSION` stays 1, so mixed desktops keep talking to each other. The new `mesh_members.role` column syncs like any column, and older nodes skip it.

## 2. Gateways

The phone holds one connection, to any full member it can reach. That member is its **gateway**. The gateway answers the phone's reads from its own synced database, runs chat turns for it, passes Code commands to each session's host (through relays if needed), and forwards the live events the phone follows.

### Choosing a gateway

The phone keeps every full member's record and known addresses in `remote.json`, refreshed from the gateway after each connect (`remote.members`: records, the addresses that gateway knows, and who it reaches). A member's address is recorded when it joins, so a phone can find it before it reconnects. The phone accepts a record only when an approver it already trusts signed it.

On start, when the connection drops, and on every network change, the phone dials all known addresses at once, one per member first, staggered by 250 ms in order of preference: the last gateway, then members its gateway reported online, then by last round trip. It keeps the first connection whose handshake completes and drops the rest. On a LAN that is usually the nearest desktop. Away from home, it is whichever member answers first over ZeroTier. With no member reachable, it retries with backoff up to 30 seconds.

Every member receives every event, so any gateway can serve a resume.

### Gateway requests

All are `Rpc` messages from the phone to its gateway (`src-tauri/src/gateway.rs`). A phone may call only these and `mesh.file`.

| Method | Purpose |
| --- | --- |
| `remote.index` | Conversations, Code sessions, each session's live state (status, mid-turn, open permission requests) and the replies streaming. |
| `remote.catalog` | Providers (without keys), models, the default model and the default system prompt. |
| `remote.members` | Member records, known addresses of full members, and which of them this gateway reaches. |
| `remote.messages`, `remote.switch_branch` | A conversation's visible branch, and switching branches. |
| `remote.create_conversation`, `remote.update_conversation` | New chats; title, model, system prompt and pin. |
| `remote.chat_send`, `remote.chat_regenerate`, `remote.chat_cancel` | Chat turns, run by the gateway or the computer the phone picked (section 4). |
| `remote.streams` | Replies streaming anywhere, for a chat opened mid-reply. |
| `remote.code` | A Code command (`code.prompt`, `code.permission`, `code.cancel`, …) run on the session's host. |
| `remote.transcript`, `remote.usage`, `remote.update_code_session` | The transcript cache, usage, and the title. |
| `remote.attach_put`, `remote.thumbnail` | Upload an attachment in 3 MB chunks; an image's thumbnail. |
| `remote.subscribe`, `remote.unsubscribe` | Follow topics, each from a point (section 3). |
| `remote.admit`, `remote.reject` | Decide a join request waiting on any member (section 7, "Join requests"). |

`remote.code` names the phone as the prompt's origin, so the phone shows its own prompts and every other window shows them as the phone's.

## 3. Resuming: state sync, after mosh

Mosh's State Synchronization Protocol has the client acknowledge state N. The server then sends one diff from N to the current state, never a replay of every step in between. Patchbay uses the same idea per topic, over whatever transport carries the connection (`crates/patchbay-mesh/src/remote.rs`).

### Topics and points

- `session:<id>`: one Code session's live events, numbered by its host.
- `chat:<id>`: one conversation's live stream, numbered by its streamer.
- `index`: list changes (`remote://index-changed`) and live session summaries (`remote://live`). Always followed.

The node an event comes from stamps it with a point `_pt: { s, n }`: a stream id and a sequence number from 1. A new stream starts whenever its topic starts over: an agent starting, a reply starting, or the session running on a new host. Older windows ignore the field.

Every full member keeps a ring of each topic's current stream, in memory, up to 8 MB per topic, dropped after two hours untouched. Members already receive these events, so this costs no extra traffic. A gap in the numbers (missed while disconnected) starts the ring over.

### Subscribe

`remote.subscribe` names each topic with the last point the phone applied, or none. The gateway registers the topic at once, so live events follow the reply in order, and answers each topic with:

- **Delta**, when the point is in the ring's stream and the ring still reaches back to it: the events since, merged (below).
- **Snapshot** otherwise. For a session: the host's attach replay (ready state, open permission requests, usage), plus the ring's events since the last finished turn (the `tail`, prompts marked `replay`). The phone lays these over the transcript cache, which a window saves at the end of each turn. A `covered: false` tail started after the ring lost its beginning, and the phone notes that earlier output is missing. For a chat: the stream so far.

The phone holds back live events for a topic while its subscribe is in flight, then applies those after the answer's point. The desktop's own windows hold back events the same way while they attach to a running session, which also fixes a race they had.

The phone keeps points in memory only. If Android ends the app, its views start over anyway.

### Merging

A delta is the state change, not the history of it:

- neighbouring text and thinking chunks join;
- updates to one tool call fold into its first appearance;
- only the latest usage and config remain;
- permission requests answered meanwhile vanish, with their answers.

Coming back after an hour of agent work is one small message per open topic, not an hour of tokens.

### Fast reconnect

After each connect, the gateway gives the phone a **resume ticket**: a random 16-byte id and 32-byte key, valid for 24 hours, bound to the phone's static key. The gateway keeps the newest four per phone, in memory. A reconnect to that gateway skips SPAKE2 and runs Noise `IKpsk2` with the ticket key, the gateway's static key known from its record. That is one round trip instead of three. An unknown ticket (expired, or the gateway restarted) gets a plaintext refusal, and the phone falls back to the passphrase handshake on the same address. Only the dialer can tell a wrong key, since `psk2` enters at the second message; the gateway learns it when the phone's first message fails to decrypt.

## 4. Chats from a phone

The phone has no provider keys, so the gateway makes the LLM call. `remote.chat_send` carries the conversation, the model, the text, attachment ids and the parent (absent: after the visible branch; empty: a new first message). The gateway runs the turn as the streamer, and its own window shows it too, credited to the phone. From there it is an ordinary mesh chat: the stream goes to every member, the finished message syncs, and stop works from any node.

The phone can pick another computer to run a chat, for its provider keys or its network: the "Runs on" control in the composer. The phone keeps the pick per chat and sends it as `node`. The gateway passes the turn to that computer as `chat.run`, which waits briefly for the new conversation and attachments to sync there, then runs the turn as the streamer, credited to the phone in its own window. A picked computer that is offline is refused with an error, not silently replaced by the gateway.

If the gateway drops mid-reply, the reply keeps going there. The phone's new gateway receives the stream like any member, and the phone resumes the `chat:<id>` topic. A phone network change never interrupts a reply.

v1 covers new chats, replies, stop, regenerate, model and effort choice, attachments, editing (a new branch) and moving between branches. The phone's picker opens the system file and photo chooser, and shrinks photos to the 2048-pixel edge the model gets anyway before uploading.

### Starting a Code session

The phone picks a computer, an agent and a folder on that computer, and the session runs there as if it had been started at that computer's own screen. Three gateway requests carry it, each naming the computer in `host`. The gateway answers them itself when it is that computer, and otherwise passes them on over the mesh. Those mesh methods are member-only; phones reach them only through their gateway.

- `remote.new_session_options` (mesh: `code.new_options`): the agents installed on that computer, its default agent, its home folder, and up to 12 folders that recent sessions hosted there used and that still exist.
- `remote.list_dirs` (mesh: `fs.dirs`): the folders in one directory. It lists folders only, never files. It hides hidden folders, stays inside the home folder (after resolving symlinks), and returns at most 500.
- `remote.create_code_session` (mesh: `code.create`): creates the session on that computer, with the same checks as the desktop's folder dialog. The row syncs to every member, and the phone then opens it, which starts the agent there.

An offline computer is refused with its name.

## 5. Transport

The phone uses the existing WebSocket transport, unchanged.

The expected setup is a member on the same LAN at home, and ZeroTier away from home. ZeroTier already handles roaming below Patchbay: the phone's ZeroTier address stays the same across network changes, so a TCP connection inside it survives short gaps. What a WebSocket can't survive is Android suspending the app or a move between the LAN and ZeroTier. Gateway racing, resume tickets and state sync handle both.

A UDP transport like mosh's would mean owning congestion control, packet sizing and fragmentation for transcripts and attachments far larger than a terminal screen. If measurements ever show reconnects are still slow, the next step is QUIC (`quinn`): connection migration, 0-RTT resumption and independent streams, with certificates built from the Ed25519 key in each member record. The message layer is transport-agnostic, so QUIC could sit beside WebSocket without protocol changes. It isn't planned.

## 6. App structure

- **`patchbay-mesh` crate.** The handshakes, wire messages, member records, key helpers, the resume logic and `client::RemoteClient` (the phone's connection manager). No Tauri, SQLite, agent or PTY dependencies. The desktop app and the phone app both use it, and the desktop's tests drive a `RemoteClient` against real desktop nodes.
- **`remote/src-tauri`.** The phone app's backend. It answers the commands the desktop's backend does (`list_conversations`, `send_chat_message`, `code_attach`, `code_prompt`, …) by asking the gateway, and emits the same events (`code://event`, `mesh://chat`, `mesh://changed`). So the desktop's chat and Code views run on the phone unchanged, with desktop-only controls (export, terminal, move, take over) hidden by `isRemoteApp()`.
- **`src/remote/`.** The phone layout (`remote.html`, built to `dist-remote/`):
  1. **Join:** address, passphrase, device name, then the confirmation code.
  2. **Home:** Chats and Code tabs. Code sessions are grouped by host, with Working, Ready, Offline and an amber "Needs you", whose count also shows on the tab.
  3. **Chat** and **Code** views: the desktop's, with a back button, behind the system back gesture.
  4. **Settings:** connection, computers, lock, staying connected in the background, leaving the mesh.
- **Keys.** The phone's identity keys, passphrase and resume tickets are sealed with an AES key the Android Keystore holds (`remote/src-tauri/src/keystore.rs`). Android backups are off, since the Keystore key can't be restored anyway. iOS would use the Keychain behind the same calls.
- **Development.** `fakeBackend.ts` answers the phone's commands too, so `pnpm dev:remote` runs the phone layout in a browser.

## 7. Platform specifics

The Android parts sit in a small Tauri plugin (`remote/plugin`) with three calls: `seal`, `open` and `keepAlive`. An iOS plugin can implement the same calls.

- **Network changes.** The backend checks the network interfaces' addresses every two seconds, in Rust, so iOS gets it too. A change makes the client ping its gateway at once and race for another if it doesn't answer within four seconds, instead of waiting for heartbeats to time out.
- **Background.** When the app leaves the screen while an agent or a reply runs, a foreground service with a quiet notification keeps the process and its connection alive (Android 15 limits these to six hours a day). The backend raises local notifications for permission requests, finished turns and finished replies while the app is out of sight. Turning "Stay connected in the background" off, or on iOS, the app reconnects and resumes when it returns.
- **Join requests.** Every gateway sends its phones the `mesh://join-request` and `mesh://join-resolved` events described in mesh.md, and the list of open requests when a phone connects. The client believes a request only from a host it has a record for, with a matching code. A phone approves with its own identity: it signs the admission record (`RemoteClient::approve`) and the gateway passes it to the host as `mesh.admit`, which accepts a phone's signature as a member's. Out of sight, the backend raises a notification per request with Approve and Decline actions and the code in its text (`tauri-plugin-notification` action types). On Android every action opens the app, so the backend keeps what the user chose until the window asks (`remote_take_join_action`), which is after the lock screen if one is on. Tapping the notification opens Settings, which lists the requests with their codes.
- **Heartbeats.** The phone pings every 15 seconds on screen and every 60 seconds in the background, and gives up after three missed. The gateway drops a phone silent for 200 seconds.

## 8. Releases and the F-Droid repository

Each release publishes the APK in two places: attached to the GitHub release, and in a self-hosted F-Droid repository on GitHub Pages. Any F-Droid client (F-Droid, Droid-ify, Neo Store) can add that repository and update the app like any other. Obtainium also works straight from the GitHub releases, with no extra setup.

- **Build** (`build.yml`, job `android`, on the release PR). `tauri android build --apk --target aarch64`, unsigned. Tauri derives the `versionCode` from the version (`major * 1000000 + minor * 1000 + patch`), so every release goes up. The job is best effort, like Windows and macOS. `checks.yml` builds the phone's frontend and checks its backend on every PR.
- **Sign** (`release.yml`, before the GPG step). `zipalign` and `apksigner` with the key in `PATCHBAY_APK_KEYSTORE` (base64 PKCS12) and `PATCHBAY_APK_KEYSTORE_PASS`. This key can never change: Android refuses an update signed by a different key. Without the secret, the release goes out without the phone app.
- **Publish to GitHub.** `patchbay-remote-<version>-arm64.apk` on the release, GPG-signed like the other packages.
- **Update the repository** (`scripts/ci/fdroid-repo.sh`). `fdroidserver` 2.4.5 from PyPI (Ubuntu's 2.2 can't read current APKs) runs `fdroid update` over what `gh-pages` already has plus the new APK and `remote/fdroid/metadata/dev.patchbay.remote.yml`, signing the index with the repository key (`PATCHBAY_FDROID_KEYSTORE`, `PATCHBAY_FDROID_KEYSTORE_PASS`, alias `patchbay-fdroid`). It keeps the newest three versions, deletes the archive, and force-pushes `gh-pages` as one fresh commit, so the branch stays a few APKs, well under Pages' 1 GB limit.
- **Adding it on a phone.** `https://jpsutton.github.io/patchbay/fdroid/` has the repository address, its fingerprint and an "Add to F-Droid" link (`fdroidrepos://…?fingerprint=…`), so a client checks the index key on first add.

Listing in the main F-Droid repository is separate and can come later. F-Droid builds from source on its own servers, which for a Tauri app means a recipe that installs Rust, the Android NDK and Node. Unless the build is reproducible, F-Droid signs that build with its own key, so users can't switch between the two sources without reinstalling.

Human time: creating the APK signing key and the repository key, backing both up, adding the four secrets, and turning on GitHub Pages for `gh-pages`. See remote/README.md.

## Open questions

- Whether the phone keeps a small encrypted on-disk cache of recently viewed transcripts, so the app shows something before the first round trip.
