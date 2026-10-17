# Patchbay Remote: design

A phone app that drives a mesh's chats and Code sessions. It does no work of its own: no agents, no LLM calls, no synced database. Android comes first. Everything outside the Android-specific parts is written so that an iOS client can follow without changes to the protocol. This builds on [mesh.md](mesh.md); read that first.

## Goals

1. **Thin.** The phone joins the mesh, then views and drives what other members run. It stores no provider keys and no copy of the database.
2. **Chats and Code sessions in v1.** List them, read them live, send, stop, and answer permission prompts.
3. **Fast resume.** Opening the app after a disconnect, a network change or hours in the background shows the current state in about one round trip. That holds whatever happened in between.
4. **Survives network changes.** Moving from home Wi-Fi (a member on the LAN) to cellular with ZeroTier switches gateways without losing anything.

## Non-goals

- Hosting Code sessions, running agents, or making LLM calls on the phone.
- Push notifications through Apple or Google services. They would put a cloud broker back in. See section 7.
- A custom UDP transport. See section 5.
- Terminal, diff and file views, and starting moves or take-overs, in v1.

## 1. Remote members

A phone joins with the same SPAKE2 and Noise handshake and the same six-digit approval as any node. Its admission record carries `role: "remote"`, signed like the rest of the record. A desktop member is `role: "full"`, the default for records without the field.

A remote member:

- never receives the mesh data key or `mesh_secrets`, so a lost phone leaks no provider keys;
- doesn't take part in database sync (`SyncVv`, `SyncOps`);
- isn't a host, a move or take-over target, a checkpoint mirror or a relay;
- appears in Settings → Mesh with a phone badge, and is removed the same way as any member.

Remote control is remote code execution on every host, the same as for a desktop member. Admission stays the trust boundary. The phone adds a biometric or device-credential lock when the app opens (`tauri-plugin-biometric`), because phones get lost more often than laptops.

**Version gate.** Older nodes would ignore an unknown `role` field and admit the phone as a full member. To prevent that, `PROTOCOL_VERSION` becomes 2. The phone refuses any peer whose `Hello` reports a lower version, and full members refuse to send the data key or sync to a remote record. A remote member therefore needs every gateway it uses to run a version with this design.

## 2. Gateways

The phone holds one connection, to any full member it can reach. That member is its **gateway**. The gateway answers the phone's reads from its own synced database. It forwards Code commands to their host through the existing `MeshRpc` routing, including relay circuits. It forwards live events the phone has subscribed to.

Hosts and streamers need nothing new beyond section 3. They already send their events to every member.

### Choosing a gateway

The phone knows each member's addresses from `Hello`, link reports and `mesh.addrs`, such as a LAN address and a ZeroTier address. The phone saves this list locally, since it has no synced database.

On start, on resume and on every network change, the phone dials the known addresses of all full members at once, with a 250 ms stagger in lowest-latency-first order. It keeps the first connection whose handshake completes and closes the rest. On a LAN that is usually the nearest desktop. Away from home, it is whichever member answers first over ZeroTier.

If the gateway drops, the phone repeats the race and resumes on the new gateway (section 3). Every member receives every event, so any gateway can serve a resume.

### Gateway RPCs

All are `Rpc` messages from the phone to its gateway.

| Method | Purpose |
| --- | --- |
| `remote.index` | Summary rows for conversations and Code sessions since an index version (section 3). |
| `remote.conversation` | One conversation's message tree, or the part after a given message. |
| `remote.session` | A Code session's transcript cache, host, status and open permission requests. |
| `remote.models` | Models and providers the mesh has, without keys. |
| `remote.subscribe` | Set the topics to forward, each with a resume point (section 3). |
| `chat.send`, `chat.stop`, `chat.regenerate` | Chat actions, run by the gateway (section 4). |
| `code_*` | The existing Code commands, forwarded to the host as today. |
| `attach.put` | Upload an attachment in 4 MB chunks, stored by SHA-256 like any attachment. |
| `mesh.file` | Fetch attachment bytes, as today. |

## 3. Resuming: state sync, after mosh

Mosh's State Synchronization Protocol has the client acknowledge state N. The server then sends one diff from N to the current state, never a replay of every step in between. Patchbay uses the same idea per topic, over whatever transport carries the connection.

### Topics and sequence numbers

- `session:<id>`: one Code session's live events. The host numbers each event `(epoch, seq)`, where `seq` restarts at 0 when `epoch` changes on a move or take-over.
- `chat:<id>`: one conversation's live stream. The streamer numbers each event `(stream_id, seq)`.
- `index`: changes to the conversation and session lists. The gateway uses its sync high-water mark as the index version.

Every full member keeps a bounded ring of recent events per live session and stream topic, held in memory and capped at 2 MB per topic. Members already receive these events, so this costs no extra traffic.

### Resume

`remote.subscribe` names each topic with the last point the phone holds, or none. For each topic, the gateway answers with one of:

- **Delta.** The events after that point, merged (below), when its ring still reaches back that far.
- **Snapshot.** The current state, when the ring doesn't reach back or the epoch or stream changed. For a session, that is the transcript cache, status and open permission requests, the same data `code_attach` replays today. For a chat, that is the committed messages plus the stream buffer.

Live events then follow on the same topic. The phone acknowledges the last point it applied with each `remote.subscribe` and every 5 seconds while it is in the foreground. It keeps the last acknowledged point per topic in memory, and on disk for the topics on screen.

### Merging

A delta is the state change, not the history of it:

- consecutive text and thinking chunks of one message become one chunk;
- tool-call updates collapse to the latest state of each call;
- status, usage and mode updates keep only the latest;
- permission requests that were answered meanwhile are dropped, along with their answers.

Coming back after an hour of agent work is one small message per open topic, not an hour of tokens.

### Fast reconnect

After a full handshake, the gateway issues a **resume ticket**: a random id and a 32-byte key derived from the Noise session, valid for 24 hours and bound to the phone's node id. A reconnect to the same gateway skips SPAKE2 and runs Noise `IKpsk2`, with the ticket key as the pre-shared key and the gateway's static key already known from its member record. That takes one round trip instead of three. A ticket works only at the gateway that issued it. A reconnect to a new gateway runs the full handshake, still fast on a LAN or ZeroTier.

## 4. Chats from a phone

The phone has no provider keys, so the gateway makes the LLM call. `chat.send` carries the conversation id (or none for a new chat), the parent message id, the text, attachment hashes and the model. The gateway writes the user message, starts the reply as the streamer, and returns the ids. From there it is an ordinary mesh chat: the stream goes to every member, the finished message syncs, and stop works from any node.

If the gateway drops mid-reply, the reply keeps going there. The phone's new gateway receives the stream like any member, and the phone resumes the `chat:<id>` topic. That is the existing streamer design, so a phone network change never interrupts a reply.

v1 covers new chats, replies, stop, regenerate, model choice, photo and file attachments, and moving between sibling branches of the message tree. Editing past messages and projects can follow.

Images are resized on the phone to the size the desktop composer sends, before `attach.put`.

## 5. Transport

The phone uses the existing WebSocket transport, unchanged.

The expected setup is a member on the same LAN at home, and ZeroTier away from home. ZeroTier already handles roaming below Patchbay: the phone's ZeroTier address stays the same across network changes, so a TCP connection inside it survives short gaps. What a WebSocket can't survive is Android suspending the app or a move between the LAN and ZeroTier. Both are handled by gateway racing, resume tickets and state sync, not by the transport.

A UDP transport like mosh's would mean owning congestion control, packet sizing and fragmentation for transcripts and attachments far larger than a terminal screen. If measurements ever show reconnects are still slow, the next step is QUIC (`quinn`): connection migration, 0-RTT resumption and independent streams, with certificates built from the Ed25519 key in each member record. The message layer is transport-agnostic, so QUIC could sit beside WebSocket without protocol changes. It isn't planned.

## 6. App structure

- **`patchbay-mesh` crate.** `handshake`, `wire`, `keys`, `members` and the client side of the remote protocol move out of `src-tauri` into a workspace crate with no Tauri, SQLite, agent or PTY dependencies. The desktop app and the phone app both depend on it.
- **`remote/` Tauri app.** A separate Tauri 2 project for Android, later iOS, depending on `patchbay-mesh`. Its Rust side holds the gateway connection, the topic state and the resume tickets, and it exposes commands and events to the UI the same way the desktop backend does.
- **Frontend.** A second Vite entry (`remote.html`) in the same `src/` tree. It reuses the store's event reducers (`applyCodeEvent`, chat stream handling), message and transcript rendering, permission prompts and the model picker, inside a mobile layout:
  1. **Join:** address, passphrase, device name, then the confirmation code.
  2. **Home:** two tabs, Chats and Code. Code sessions are grouped by host, with a live status dot and the amber "needs you" mark, sorted by last activity.
  3. **Conversation** and **Session** views: live transcript, composer with attachments, stop, model (and mode for Code), permission prompts.
- **Keys.** The phone's identity keys and resume tickets are encrypted under an AES key held in the Android Keystore, behind the `KeyStore` trait. iOS would use the Keychain behind the same trait.
- **Development.** `fakeBackend.ts` gains the remote commands, so the mobile layout runs in a desktop browser.

## 7. Android specifics

Everything here sits behind a small Tauri plugin interface (`remote-platform`) that an iOS plugin can implement later.

- **Network changes.** `ConnectivityManager.registerDefaultNetworkCallback` notifies the Rust side, which starts a gateway race at once instead of waiting for heartbeats to time out.
- **Background.** While any shown session or chat is running, an optional foreground service with a persistent notification keeps the gateway connection open. It raises a local notification when a permission prompt or a finished turn arrives. Without the service, the app reconnects and resumes when it returns to the foreground. iOS has no equivalent and would always use the resume path.
- **Heartbeats.** `Ping` every 15 seconds in the foreground and every 60 seconds under the service, with a timeout of three missed pings.
- **Distribution.** See section 9.

## 9. Releases and the F-Droid repository

Each release publishes the APK in two places: attached to the GitHub release, and in a self-hosted F-Droid repository on GitHub Pages. Any F-Droid client (F-Droid, Droid-ify, Neo Store) can add that repository and update the app like any other. Obtainium also works straight from the GitHub releases, with no extra setup.

- **Build** (`build.yml`, on the release PR, beside the desktop packages). `tauri android build --apk` for `arm64-v8a`, unsigned. The `versionCode` comes from the version: `major * 1000000 + minor * 1000 + patch`, so every release goes up. Other ABIs can be added later as split APKs with their own version codes.
- **Sign** (`release.yml`, with the GPG step). `apksigner` with the APK signing key from a repository secret. This key can never change, because Android refuses an update signed by a different key. Its backup matters as much as the GPG key's.
- **Publish to GitHub.** The signed APK goes on the release beside the desktop packages.
- **Update the repository.** `fdroidserver` from Ubuntu's packages runs `fdroid update` over `fdroid/repo/` on the `gh-pages` branch. It writes the signed index (`index-v1.jar`, `entry.jar`, `index-v2.json`) from the APKs there and `fdroid/metadata/dev.patchbay.remote.yml` (name, summary, description, license, source and issue URLs, and the screenshots). The index is signed with a second key, the repository key, also kept in a repository secret.
- **Bounded size.** F-Droid clients fetch APKs from the repository itself, so they can't point at release assets. `archive_older: 3` keeps the last three versions in the repository, and the job force-pushes `gh-pages` as one fresh commit each release. That keeps the branch at a few APKs and well under GitHub Pages' 1 GB limit.
- **Adding it on a phone.** The repository URL is `https://jpsutton.github.io/patchbay/fdroid/repo`. The README and the user guide give that URL, its fingerprint, and a QR code of `fdroidrepos://…?fingerprint=…`, so a client checks the index key on first add.

Listing in the main F-Droid repository is separate and can come later. F-Droid builds from source on its own servers, which for a Tauri app means a recipe that installs Rust, the Android NDK and Node. Unless the build is reproducible, F-Droid signs that build with its own key, so users can't switch between the two sources without reinstalling.

Human time: creating the APK signing key and the repository key, backing both up, and turning on GitHub Pages for `gh-pages`.

## 8. Desktop changes

- Admission and Settings → Mesh show the remote role, and the approval dialog says it is a phone.
- The `remote.*` and `chat.*` gateway RPCs, the per-topic event rings, and sequence numbers on session events (host) and chat stream events (streamer).
- Refuse sync and data-key sends to remote records.

## Phases

1. Extract `patchbay-mesh`. Add the remote role, the version gate and the gateway RPCs. Test with a headless remote client against two desktop instances, extending `mesh_app_tests`.
2. Sequence numbers, rings, `remote.subscribe` with delta and snapshot, merging, and resume tickets. Test resume after a dropped connection and after a gateway switch.
3. The `remote/` app on Android: join, home, conversation and session views.
4. The network callback, the foreground service and local notifications.
5. CI builds, release signing, and the F-Droid repository on GitHub Pages.

Human time: testing on a real Android device for phases 3 and 4. For phase 5, the keys and GitHub Pages setup listed in section 9.

## Open questions

- Whether the phone keeps a small encrypted on-disk cache of recently viewed transcripts, so the app shows something before the first round trip.
- Whether a remote member should be able to approve new members. The default is no: only full members approve.
