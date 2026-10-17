# Patchbay Mesh: remote control and session migration (design sketch)

Status: idea stage. This describes the desired end state and a plausible route there, not settled implementation.

## Goals

1. **Peer-to-peer, no cloud broker.** Nodes reach each other over L3 the user already provides: Tailscale, ZeroTier, a VPN, or a Cloudflare Tunnel. Patchbay never phones home, and no service reaches into a machine.
2. **Shared-secret membership.** A node proves it knows the mesh passphrase without sending it, and the user approves it before it joins. All data syncs across all members.
3. **Location-independent sessions.** Any node can drive a Code session hosted on another node. The user can also move a session to a different node, on purpose or after its host dies.

## Non-goals

- NAT traversal, third-party relays and discovery across the internet. The L3 layer handles reachability. Members can relay for each other (§1), but nothing outside the mesh does.
- Multi-user collaboration. One person owns the mesh. Every member trusts every other member fully.
- Live migration of running processes. Terminals, dev servers and in-flight tool calls do not move. Sessions move only at a turn boundary.

## Vocabulary

- **Node**: one Patchbay install. Each has a long-lived ed25519 identity key (`node_id` = hash of the public key) and a display name.
- **Mesh**: the set of approved nodes that share one mesh passphrase.
- **Host**: the node that runs a Code session's agent process and owns its working tree. Every Code session has exactly one host at a time.
- **Controller**: any node whose UI is driving a session. The host is usually also a controller.

## 1. Transport and membership

### Wire

One listener per node: **WebSocket over TCP**, default port 41890 (any port the user picks). WebSocket is chosen so a plain Cloudflare Tunnel HTTP hostname works with no `cloudflared` on the client side. Tailscale and ZeroTier carry it unchanged.

Every connection runs two steps inside the WebSocket:

1. **PAKE on the mesh passphrase.** Symmetric SPAKE2 (RustCrypto `spake2`, `start_symmetric`) or CPace. Both suit a shared secret with no client/server roles. The output is a 256-bit key `K`. The passphrase never crosses the wire, and a recorded handshake gives an attacker nothing to brute-force offline. Each failed attempt tests one guess, so the passphrase can be memorable as long as failures are rate-limited per source address.
2. **Noise `XXpsk3`** (Rust: `snow`) with `K` as the PSK. That adds:
   - mutual exchange of node static keys, so each peer learns the other's `node_id`;
   - encryption and forward secrecy for everything after the handshake. model-inmate's handshake is proof-of-knowledge only and leaves later traffic in the clear. That is acceptable on an isolated VLAN, but not over a tunnel whose edge terminates TLS;
   - binding of every field to the handshake transcript. model-inmate had to add a separate HMAC over its SSH key offer because the HS2 proof did not cover added fields. Noise closes that gap by construction.

The SPAKE2 transcript is fed into the Noise prologue, so the two steps cannot be spliced from different connections.

This is the same idea as model-inmate's `SHA256(secret || salt_a || salt_b || role)`, upgraded from "prove you know it" to "derive keys from it, and resist guessing."

### Admission

The passphrase alone does not make a node a member. Joining has two gates:

1. The new node completes the handshake with any member, which proves it knows the passphrase. It is now **pending**: it can see the member list and nothing else.
2. A user approves it on any existing member. The approval dialog shows the new node's name and a short code derived from both static keys. The same code is shown on the new node, so the user can confirm they match (as in Bluetooth pairing).

The approving node signs an **admission record** (`node_id`, ed25519 signing key, X25519 Noise static key, name, approver, time) and publishes it through sync. Every node checks that each admission record is signed by a node that was already a member. The member list is the source of truth for who is trusted. A non-member's connection is limited to the pending state, even if it knows the passphrase.

Because admission is an explicit act by the user, every member is fully trusted. Remote control (§3) is on by default for every member, with no per-node opt-out.

### Peer pinning and removal

A member's `node_id` is bound to the static key in its admission record. A different key under a known `node_id` is rejected, so a node that knows the passphrase cannot impersonate a member.

Removing a node publishes a signed **removal record**. Members then refuse that node's key, and they rotate the mesh data key (§2) so it cannot read later changes. Changing the passphrase is optional because the removal record already locks the node out, though it is good hygiene.

### Discovery

There is no broadcast across L3. Each node keeps a **peer address book** (`node_id`, last-known addresses). To join, the user enters one reachable address and the passphrase. After that, peers gossip addresses, so one seed is enough. On a LAN, mDNS can optionally fill in addresses.

### Relaying through a third node

Overlays are not always a full mesh. For example, a laptop on Tailscale and a desktop on ZeroTier might both reach a home server that is on both. Patchbay detects this and routes through the server.

- **Link state:** each node gossips its live links: which members it has an open session with, and the round-trip time of each. Every node builds the same small graph from these reports.
- **Detection:** when A cannot open a direct session to B, but the graph shows both have a live link to C, A has a relay path. With several candidates, A picks the shortest path by summed RTT. Paths with more than one hop work the same way.
- **End-to-end:** A runs its PAKE and Noise handshake with B *through* C. C forwards opaque frames. B's pinned key is still verified end to end, and C cannot read or alter the traffic.
- **Back to direct:** A keeps retrying the direct link in the background, with backoff, and switches to it when it comes up. The UI shows a relayed peer as "via C".

Sync does not need relaying at all. Anti-entropy is transitive: a change made on A reaches B through C as an ordinary sync between neighbors. Relaying matters for traffic addressed to one specific node: remote control, file reads on a host, and migration transfers.

## 2. Data sync

### What syncs

| Data | Sync | Notes |
| --- | --- | --- |
| Conversations, messages | yes | Messages already form an immutable tree keyed by UUID, so merging is a set union. |
| Code session metadata and transcript | yes | Includes `host_node` and the lease epoch (see §3). |
| Prompts library, MCP server configs, permission rules | yes | Permission rules are scoped by project identity, not absolute path (see §4). |
| Providers and models | yes, keys included | API keys sync sealed under the mesh data key. Each node decrypts them into its own OS keychain. |
| Attachments | on demand | Already content-addressed by `sha256`; fetch from any peer that has the blob. |
| Usage ledger | yes, per node | Append-only rows tagged with the node that spent the tokens. |
| Agent working state (`$GROK_HOME/sessions`, worktrees) | no | Moved explicitly during migration and checkpointing (§4, §5). |

### Mechanism

Use a small op-log CRDT on top of the existing SQLite database. This is the model cr-sqlite uses, but without depending on that extension.

- Each write through `db.rs` also appends `(node_id, seq, hlc, table, row_id, column, value)` to a local `changes` table. `hlc` is a hybrid logical clock.
- Immutable rows (messages, attachments, usage) are insert-only. Mutable columns (title, pinned, `active_leaf_id`, system prompt) are last-writer-wins by `hlc`. A delete writes a tombstone.
- Anti-entropy runs on connect and on every change: peers swap version vectors (`node_id → max seq`) and stream the missing ops. The UI receives the same events it gets for local writes, so remote changes appear live.

The schema is small and the data has a single owner, so this stays a few hundred lines rather than a general CRDT library.

Every change goes to every member. There is no partial replication.

### Mesh data key

Noise protects data on the wire. Secrets also need protection at rest in each node's op log. For that, the mesh has a random 256-bit **data key**:

- The approving node hands it to a new member over Noise right after admission.
- Secret values (provider API keys, `inline_key`) are written to the op log as ciphertext under this key (XChaCha20-Poly1305). A node decrypts a key only to store it in its own keychain.
- On removal, a member generates a new data key, sends it to the remaining members, and re-seals the current secrets under it. The removed node keeps whatever keys it already had. The UI should therefore list the provider keys that were synced to it and suggest revoking them at each provider.

## 3. Remote control

### Ownership lease

Every Code session row carries `host_node` and `epoch`. Only the host runs the agent. The epoch is a fencing token: every command and event carries it, and a node that sees a higher epoch for a session it thinks it hosts stops its agent at once.

### Driving a session from another node

The existing backend already has the seams this needs:

- **Events out:** `acp::spawn` emits every `CodeEvent` through a `Sink` closure (`src-tauri/src/code/mod.rs`). The host wraps that sink to push events to every connected member over the mesh, in addition to `app.emit`. Every node's sidebar therefore shows live status (working, waiting on permission, idle) for every session, not just the open one.
- **Commands in:** `AgentManager::send`, `set_model`, `set_mode`, `rewind` and `respond_permission` become RPCs. A controller's UI calls the same Tauri commands. When the session's host is another node, the backend forwards the call there instead of to a local `AgentCmd` channel.

So the frontend barely changes: a session shows which node hosts it, and everything else behaves as it does locally. Permission prompts appear on every attached controller. The first answer wins, and the others are dismissed.

The terminal panel and the diff and file views read the host's filesystem through the same RPC path.

### Trust

Remote control is on for every member, with no per-node switch. The agent runs shell commands on its host, so remote control amounts to remote code execution. The trust boundary is admission (§1): a node is a member only because the user approved it. Agent permission prompts still apply to remote controllers exactly as they do locally.

### Live chat streams

Rule for the whole mesh: **everything is as live as possible.** A reply streaming on one node streams on every node, not just once it finishes.

Plain chats have no host. The node that makes the LLM call for a reply is that reply's **streamer**, only while the reply is generating.

- **Deltas are ephemeral.** The streamer pushes each `ChatEvent` (text, reasoning, usage, done) to every connected member as it arrives, tagged with the conversation and message ids. These events skip the op log. Only the finished message is committed, through sync, as one insert.
- **Late joiners catch up.** A node that connects, or opens the chat, mid-stream asks the streamer for its buffer so far, then follows the live deltas.
- **Stop works anywhere.** Stop on any node is forwarded to the streamer's `cancel`. That covers both the stop button in another node's UI and a node reached only through a relay.
- **Concurrent sends are safe.** If two nodes send in the same chat at once, both messages become siblings in the existing message tree. Neither is lost.
- **Streamer dies mid-reply.** The other nodes still hold the partial text from the deltas. When the streamer's link drops and no `done` has arrived, the first node to notice commits the partial reply with an "interrupted on <node>" error. The commit reuses the reply's message id, so if several nodes notice at once, their inserts merge into one row. Regenerate works as usual.

## 4. Planned migration

Moving a session from host A to node B, at a turn boundary:

1. **Quiesce.** A waits for the agent to go idle, with no pending permission request, then stops it with `stop_and_wait`.
2. **Snapshot the workspace.** A records the uncommitted state without touching the user's branch: `git stash create` plus the untracked files, written to a ref such as `refs/patchbay/handoff/<session>`. A then sends a `git bundle` of that ref and the commits B lacks. A non-git folder falls back to a tarball.
3. **Map the path.** Project identity is the git remote URL plus the path inside the repo, not the absolute path. B resolves that identity to its own checkout, or asks the user where to clone it. Per-node path mappings are remembered.
4. **Move the agent state.** This depends on the harness, so it lives behind the harness adapter:
   - *Grok Build adapter:* copy the session's directory from `$GROK_HOME/sessions/<url-encoded cwd>/<id>`, rewrite embedded paths, then `session/load` on B.
   - *Generic ACP fallback:* `session/new` on B, seeded with a hand-off prompt built from the synced transcript (a summary plus the last few turns). This is lossy, but it works with any ACP agent, which keeps the Code tab harness-agnostic.
5. **Hand over the lease.** A writes `host_node = B, epoch + 1` and syncs it. B starts the agent and resumes the session. A keeps the handoff ref until B confirms.

The user sees a "Move to…" action on the session, a progress step per stage, and the session continuing on B.

## 5. Surviving a dead host

A dead host cannot run step 1 or step 2, so recovery has to rely on data that was already copied out.

- **Continuous transcript:** already covered by sync. The conversation is never lost.
- **Checkpoint replication:** after each completed turn, the host pushes a workspace snapshot (step 2) and an agent-state blob (step 4) to every member, like all other changes. This uses the same code as migration and runs in the background. Bundles are incremental (only objects a peer lacks), so the steady-state cost is the size of each turn's edits.
- **Adopt:** on another node, "Take over session" claims the lease at `epoch + 1` from the latest checkpoint. Only the in-flight turn is lost.

Takeover is **manual, not automatic**. The mesh cannot tell a dead host from a network partition. If the old host comes back, it sees the higher epoch and stops its agent. It also keeps any work it did after the checkpoint on a `refs/patchbay/orphaned/<session>` ref, so nothing is silently discarded.

## Phasing

1. **Identity and transport:** node keys, SPAKE2 then Noise over WebSocket, admission with a confirmation code, member list in Settings.
2. **Sync:** op log, anti-entropy, mesh data key, read-only view of sessions hosted elsewhere.
3. **Remote control:** event fan-out, RPC forwarding, link-state gossip and relaying, a host badge in the UI.
4. **Planned migration:** git bundle handoff, path mapping, both harness adapters.
5. **Checkpoints and adopt:** background replication, fencing, orphaned refs.

Each phase is useful by itself: phase 2 alone gives "my chats on every machine."

## Later

- **Phone and browser access.** Deferred. A node could serve the frontend (which already runs outside Tauri against `fakeBackend.ts`) to a browser on the overlay. That browser would need a SPAKE2 and Noise client in JS, or a per-node pairing token over HTTPS.
