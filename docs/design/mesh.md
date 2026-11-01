# Patchbay Mesh: design

How Patchbay links one person's computers: membership, data sync, remote control of Code sessions, and moving sessions between computers. For using it, see [../mesh.md](../mesh.md). The code lives in `src-tauri/src/mesh/` (network and sync), `src-tauri/src/handoff.rs` (moving workspaces) and `src-tauri/src/code/agent_state.rs` (moving agent state).

## Goals

1. **Peer-to-peer, no cloud broker.** Nodes reach each other over L3 the user already provides: Tailscale, ZeroTier, a VPN, or a Cloudflare Tunnel. Patchbay never phones home, and no service reaches into a machine.
2. **Shared-secret membership.** A node proves it knows the mesh passphrase without sending it, and the user approves it before it joins. All data syncs across all members.
3. **Location-independent sessions.** Any node can drive a Code session hosted on another node. The user can also move a session to a different node, on purpose or after its host dies.
4. **As live as possible.** Agent output and chat replies show on every node while they happen, not once they finish.

## Non-goals

- NAT traversal, third-party relays and discovery across the internet. The L3 layer handles reachability. Members relay for each other, but nothing outside the mesh does.
- Multi-user collaboration. One person owns the mesh, and every member trusts every other member fully.
- Live migration of running processes. Terminals, dev servers and in-flight tool calls don't move. Sessions move only between turns.

## Vocabulary

- **Node**: one Patchbay install. It has a random `node_id` (in `sync_meta`), an X25519 key for Noise and an Ed25519 key for signing records (both in `mesh/identity.json`, mode 0600), and a display name.
- **Mesh**: the set of approved nodes that share one passphrase and one `mesh_id`.
- **Host**: the node that runs a Code session's agent and owns its working tree. Each session has exactly one host, recorded in `code_sessions.host_node`.

## 1. Transport and membership

### Wire

Each node runs one listener: **WebSocket over TCP**, `0.0.0.0:41890` by default. WebSocket lets a plain Cloudflare Tunnel hostname work with no `cloudflared` on the dialing side, and overlays carry it unchanged.

Every connection runs two steps inside the WebSocket (`mesh/handshake.rs`):

1. **Symmetric SPAKE2** (RustCrypto `spake2`) on the passphrase, giving a 32-byte key `K`. The passphrase never crosses the wire, and a recorded handshake gives an attacker nothing to test guesses against offline. Each guess costs a live attempt, and the listener doubles a source's wait after every failure (up to 5 minutes). That makes a memorable passphrase acceptable.
2. **Noise `XXpsk0`** (`snow`) with `K` as the pre-shared key and both SPAKE2 messages in the prologue. That adds mutual exchange of static keys, encryption with forward secrecy, and binding of every later field to the handshake.

`psk0` mixes `K` in from the first message, so a wrong passphrase fails on message 1 and no static key is revealed to a non-member. The responder then sends a short plaintext marker, so the dialer can tell "wrong passphrase" from a network failure.

After the handshake, each JSON message (`mesh/wire.rs`) is split into Noise transport frames of up to 64 KiB.

This is model-inmate's `SHA256(secret || salt_a || salt_b || role)` idea, upgraded from "prove you know it" to "derive keys from it, and resist guessing". It also closes model-inmate's gap where fields added to HS2 weren't covered by its proof.

### Admission

Knowing the passphrase doesn't make a node a member:

1. The joining node completes the handshake with any member and says it is joining. It becomes **pending** on that member and receives nothing but a `JoinPending` message.
2. Both screens show a six-digit code derived from both nodes' static keys. The member the node is connected to (its **host**) announces the request to every other member and phone as a `mesh://join-request` event (`JoinRequest`: the keys, the code, the host), and `mesh://join-resolved` when it's decided. They are events, not messages, so older nodes ignore them. A member that connects later gets the open requests on connect, and a phone gets them when it connects to its gateway. Each receiver believes a request only from its host, and only if the code is the one the host's key and the joiner's key give, so the code the user compares is tied to the key that gets signed.
3. **Any** member, or any phone, can then approve or decline. The approver signs an **admission record** (`mesh_id`, `node_id`, both public keys, approver, time) with its own key. The host gets it by `mesh.admit` (a phone's gateway passes it on as `remote.admit`) and checks that it matches the request, that its approver is a member in good standing and that the signature holds. Only then does it write the record into the synced `mesh_members` table and send the joiner every member record and the mesh data key. A record that doesn't match the request can't be replayed for other keys. `mesh.reject` and `remote.reject` decline. The first decision wins; later ones get "that request is gone". If the host goes away, the other members drop its requests, and the host announces them again when it reconnects.

Records are checked against the members a node already trusts (`members::verify_record`). Only a mesh's founder signs its own record. A member admitted elsewhere whose record hasn't synced yet presents it in its `Hello`, signed by an approver the other side already knows.

### Pinning and removal

A member's `node_id` is bound to the keys in its record. A connection that presents other keys under that id is refused, so knowing the passphrase doesn't let a node impersonate a member. A joining node with a known id but new keys (a reinstall) goes back through approval.

Removal signs `removed_by`/`removed_at` into the record. The remover rotates the data key. Connections to the removed node close two seconds later, so the removal record reaches it first and it stops itself.

Leaving is a removal the member signs for itself (`removed_by` is its own id). A computer that leaves stores that record and waits half a second, so sync carries it to the connected members before it stops. A phone sends it to its gateway as `remote.leave`, and the gateway stores it and lets it sync on. Members drop a node that left from their lists. A node removed by someone else stays listed as removed. A node that leaves while it can't reach any member stays listed until a member removes it.

### Discovery

There is no broadcast across L3. A node learns peer addresses from:

- the address the user entered to join, and any extra seeds (`mesh.seeds`);
- each `Hello`: the peer's configured public addresses, plus its address as seen on an inbound connection with its listen port;
- **link reports**, flooded through the mesh, which carry each node's public addresses and the URL it reached each direct peer at.

Known addresses persist in `settings` (`mesh.addrs`). A connector loop dials every member without a direct connection every 3 seconds, with exponential backoff per URL.

### Relaying through a third node

If A can't reach B but both reach C:

- Each link report lists the reporter's direct sessions and their round-trip times. Every node holds the same small graph.
- With no session to B, A picks the first hop on the lowest-latency path (Dijkstra) and opens a **circuit**: frames go as `Relay { circuit, from, to, ttl }` messages that each hop forwards toward `to`.
- A runs the full SPAKE2 and Noise handshake with B through the circuit, so B's keys are verified end to end and C only sees ciphertext.
- A keeps dialing B directly. When a direct session comes up, it replaces the relayed one.

When two sessions to the same node exist at once (both sides dialed), both ends keep the one opened by the lower `node_id`.

Sync doesn't need relaying: a change from A reaches B through C as ordinary sync. Circuits matter for traffic addressed to one node: remote control, file fetches and moves.

## 2. Data sync

### What syncs

| Data | Notes |
| --- | --- |
| Conversations, messages, message attachments | Messages form an immutable tree keyed by UUID, so concurrent sends become siblings. |
| Attachment records | The bytes are fetched on demand from any connected peer (`mesh.file` RPC) and checked against their SHA-256. `attachments.path` stays local. |
| Code sessions | Including the cached transcript, usage, `host_node` and `epoch`. |
| Code sidebar layout | `sidebar_layout`: the user's folder groups, which group each folder is in, and the manual order of groups and folders. A folder's id names its computer (`<node_id>:<path>`), so the same path on two computers stays two entries. |
| Synced preferences | `ui_prefs`: one row per preference in `prefs::SYNCED` (group by project, send key, launch tab, reasoning, titles, chat web search, default agent, auto-archive), the value as JSON. It wins over the node's own copy. A value this build can't use (an agent only a newer node knows) leaves the node's own in place. Saving preferences writes only the synced ones that changed. |
| Prompts, MCP servers, permission rules | Permission rules keep their absolute folder, so they apply only where paths match. |
| Providers and models | `providers.inline_key` stays local. Keys travel sealed in `mesh_secrets`. |
| `mesh_members`, `mesh_secrets` | Membership and sealed secrets. |
| Not synced | The `settings` table: the other preferences (theme, text size, shortcuts, terminal, read aloud, agent paths and providers), default model, mesh configuration. Agent state, which moves only with a session. |

### Mechanism (`mesh/sync.rs`)

- Every synced cell has a clock in `sync_clock`: `(hlc, origin, seq)`. SQLite triggers on each synced table maintain them, so the rest of the app writes its tables as before. The triggers compute the hybrid logical clock in SQL and skip writes made while `sync_meta.applying` is set.
- A delete replaces the row's clocks with a tombstone (`col = '-'`). A delete beats concurrent edits of the same row, so a deleted conversation stays deleted even if another node added to it meanwhile.
- Peers exchange version vectors (highest `seq` per origin). The sender reads the clocks newer than the peer's vector, orders them by HLC (so parents come before children), and reads the current cell values from the tables. Batches never split one change, and each carries the range it covers (`upto`), so stale clocks can't stall progress.
- The receiver applies each cell only if its `(hlc, origin)` is newer than the local clock: last writer wins, per column. A row that can't be inserted yet, such as a message whose conversation hasn't arrived, is parked in `sync_parked` and retried after later batches.
- Changes go out as they happen: a watcher notices local writes within 150 ms, and received changes are forwarded to the other peers.
- Rows written before sync existed get clocks when the database opens.
- A build that doesn't know a table skips its changes but still counts them as received. So when a build syncs a table its predecessor didn't (`settings` key `sync.tables`), the node forgets its version vector and gets everything from its peers once. Changes it already has are no-ops. On a mesh of three or more full nodes where one is still on an older build, changes relayed through that node can still go missing until their writer changes them again.
- When `ui_prefs` first appears, a node brings in its synced preferences that differ from the default, under the oldest clock (`sync::seed_oldest`). Where nodes disagree, a changed setting beats a default one, and any change made after the upgrade beats both.

Every change goes to every member. There is no partial replication.

### Mesh data key

Secrets also need protection at rest in each node's database. The mesh has a random 256-bit **data key** with a generation number, kept in each member's keychain:

- `mesh_secrets` holds each secret as XChaCha20-Poly1305 ciphertext, with the keyring account as associated data, plus a keyed digest so unchanged secrets aren't rewritten. A member writes it after any key change in Settings (`secrets::reseal`). A receiving node installs it into its own keychain.
- The approver sends the key to a new member. On connect, the side with the newer generation sends it to the other. Removal rotates it and re-seals every secret.
- If two members rotate at once, the higher (generation, key) wins everywhere, and each node re-seals under the winner.
- MCP secrets stored inline (on machines without a keyring) are blanked in the synced config and travel only sealed.

A removed node keeps what it already had, so the Remove dialog tells the user to replace those keys with their providers.

## 3. Remote control

### Driving a session

Code commands (`code_start`, `code_prompt`, `code_cancel`, `code_set_model`, `code_set_mode`, `code_rewind`, `code_permission_respond`, `list_project_files`) check the session's host. For a session hosted elsewhere they become RPCs to that node, which runs the local implementation (`MeshRpc` in `lib.rs`).

On the host, every agent event goes to the local window and to every member as a mesh event. A prompt also emits a `UserPrompt` event, so windows other than the sender's show it. Permission requests appear everywhere, and the first answer wins.

A window that opens a session already running, here or elsewhere, calls `code_attach` instead of restarting the agent. The host keeps each running session's last `sessionReady`, open permission requests and latest usage, and replays them. The window shows the synced transcript cache until live events extend it.

The transcript cache (`code_sessions.transcript_json`) is one synced cell, and only the host writes it. A window on another computer hands its saves to the host (`code.save_transcript`), so a session watched only from elsewhere still gets a cache. For its view to be worth saving, that window's `code_attach` also asks the host for what the cache doesn't hold yet (`catchUp`): the events since the last finished turn from the host's event ring, or, with no cache at all, the agent's whole run while the ring holds it. These are the same as a phone's subscribe gets. When part of the turn is gone from the ring and a cache exists, the answer says `partial`. The window then keeps its view to itself rather than replace a fuller transcript. A `code_resume` snapshot (events lost) does the same.

Mesh events are sent once: those a member misses while its link to the host is down never arrive. A window that misses the end of a turn would show it running for good, with a tool call that never ends and a Stop button that does nothing. So windows catch up with `code_resume`. Each event carries its point (`_pt`, numbered per session by the host, the same points phones resume from). A window remembers the last point it applied and calls `code_resume` when:

- an event skips points;
- the mesh status changes (a link may have come back), for each shown session hosted elsewhere;
- every 30 s, for each such session mid-turn, since a relay can drop events with no status change.

The host answers with the events after the window's point while its event ring still holds them, merged as for a phone. Otherwise it sends a snapshot: the `code_attach` answer, plus the current status of each tool call the window still shows running. The host keeps those statuses for the agent's current run (`LiveAgent::tools`). A call the host doesn't know ends once the host is idle. The window holds live events back while it asks, as it does when attaching, and drops any it already has.

### Agent-to-agent messages

Patchbay gives each running Code agent a built-in MCP tool to discover unarchived Code sessions
and message one by session id. The source node routes `code.agent_message` to the target session's
host, using the same direct or relayed mesh connection as other remote-control RPCs. The target
host checks that the claimed source session belongs to the authenticated sending member, starts
the target agent if it is idle, and otherwise steers the message into the running turn or queues
it behind that turn, as it does a user's mid-turn message.
The message appears as an attributed prompt; a reply is a separate message back to the source
session. Any admitted member can use the tool, consistent with the mesh's existing full-trust
remote-control model.

The terminal panel of a remote session opens its shell on the host (`term.open` and friends). The host keys it by requester, streams output back to that node only (`mesh://term`), and the requester queues keystrokes so they arrive in order.

### Fencing

Only the host runs a session's agent. A node refuses session commands for a session its database says another node hosts. When sync moves a session's host away from a node, that node stops the session's agent. `epoch` increments on every handover and records the order of hosts.

### Trust

Remote control is on for every member. The agent runs shell commands on its host, so remote control amounts to remote code execution, and admission is the trust boundary. Agent permission prompts apply to remote windows exactly as to local ones.

### Live chat streams

The node making a reply's LLM call is that reply's **streamer** while it generates. It sends each chat event to every member as it arrives, and every node keeps a buffer per streaming conversation. Only the finished message is committed, through sync.

- A node that connects mid-reply receives the streamer's buffer as a snapshot. A window opened mid-reply asks its own node for the streams it knows.
- Stop on any node is forwarded to the streamer.
- If the streamer's connection drops and it doesn't reconnect within 15 seconds, the other nodes save the partial reply with an "Interrupted" error. They all write the same message id, so their writes merge.

## 4. Moving a session

`code_move(id, target, cwd)` runs on the target (as an RPC when the target is another node). The target pulls from the host:

1. **Package** (`handoff::package`, on the host). The host refuses mid-turn. It marks the session as moving, which blocks restarts, and stops the agent so it flushes its files. It then commits the working tree on top of `HEAD` using a private index: tracked changes plus untracked, non-ignored files, without touching the user's branch or index. The commit goes to `refs/patchbay/handoff/<session>`. A `git bundle` carries that ref, leaving out commits the target already has (the target sends its branch and remote heads). A non-git folder is a tar of up to 512 MB. The agent's state goes alongside: for Patchbay's own agent, its session file (`agent/sessions/<id>.jsonl` in the data folder); for Grok Build, its `$GROK_HOME/sessions/<url-encoded cwd>/<id>/` directory without lock files.
2. **Fetch.** The target copies the package files in 4 MB chunks (`code.fetch`).
3. **Unpack** (`handoff::unpack`). An existing checkout must be clean. A missing folder gets `git init`. The target fetches the snapshot ref, checks out the branch at the host's `HEAD`, fast-forwarding it, or creating `<branch>-from-<node>` if the local branch has diverged. It then lays the snapshot tree on the worktree (`read-tree -u --reset`) and unstages it, so the changes appear uncommitted as they were on the host. The agent's state is restored under the new path, with the old path rewritten in its files.
4. **Lease.** The target writes `host_node`, `epoch + 1`, the new `cwd`, and the ACP session id when the agent state came along. Without agent state, the session starts fresh, and the first prompt carries a hand-off note built from the transcript (`handoff::handoff_prompt`). That fallback works with any ACP agent.
5. The host deletes its package. Its moving mark clears when the lease change arrives.

The target folder is chosen by the user and defaults to the host's path.

Claude plan usage (the 5-hour and weekly windows, see `code/plan_limits.rs`) arrives only with a node's own turns. Each node broadcasts what it sees (`mesh://plan-limits`) and sends its latest to a member as soon as that member connects. A member keeps a received report only if it is newer than the one it holds, with the sender's timestamp capped at the receiver's clock. Every member's usage panel and usage guard work from the same numbers, which assumes the members share one Claude account.

## 5. Surviving a dead host

- **Transcript:** synced continuously.
- **Checkpoints:** when a turn ends, the host announces it (`mesh://checkpoint`). Every member then pulls a fresh package of kind `checkpoint`, sending the snapshot commit it already holds, so the bundle carries only what changed. Members keep the git objects in a per-session bare mirror (`mesh/mirrors/<id>.git`) and the agent state and metadata in `mesh/checkpoints/<id>/`.
- **Take over:** `code_take_over(id, cwd)` works only while the host is offline. It restores the latest local checkpoint from the mirror the same way as a move, then takes the lease at `epoch + 1`. At most the turn that was running is lost.

Take-over is manual, because the mesh can't tell a dead host from a network partition. When the old host reconnects, sync shows that it lost the lease without a move it started. It stops the agent and commits whatever is in the working tree to `refs/patchbay/orphaned/<session>-<ms>`, then tells the user.

## Known gaps

- Folder mapping between nodes is manual: the move dialog defaults to the same path. Remembering a mapping per repository (by remote URL) would save typing.
- Permission rules match absolute folders, so a rule saved on one node doesn't apply to the same project at a different path elsewhere.
- Grok session directories whose encoded path exceeds 255 bytes use a hashed name Patchbay doesn't reproduce. Those sessions use the hand-off note.
- Tombstones and parked rows are never compacted beyond a weekly expiry of parked rows.

## Later

- **Phone access.** Designed in [mobile.md](mobile.md): a remote-only member role and a phone app, Android first.
- **Browser access.** A node could serve the frontend, which already runs outside Tauri against `fakeBackend.ts`, to a browser on the overlay. That browser would need the `patchbay-mesh` handshake compiled to WebAssembly, or a per-node pairing token over HTTPS.
