# Using Patchbay across your computers

A **mesh** links the Patchbay installs on your computers. Once they are linked:

- Chats, Code sessions, saved prompts, providers, models, MCP servers and permission rules sync between all of them. A change on one computer shows up on the others within a moment.
- API keys and MCP secrets sync too, encrypted. Each computer stores them in its own system keychain.
- A chat reply that is still streaming on one computer streams on every other computer as well.
- You can drive a Code session that runs on another computer: send prompts, answer permission requests, switch models and modes, and stop it.
- You can move a Code session to another computer, with its uncommitted changes. If the computer running a session goes offline, another computer can take the session over from its last checkpoint.

Nothing goes through a cloud service. Your computers connect to each other directly.

## What you need

Your computers must be able to reach each other over the network. Patchbay doesn't provide that part; use whatever you already have:

- **Tailscale, ZeroTier or another mesh VPN.** The simplest choice: every computer gets an address the others can reach.
- **A regular VPN or a home network.** Works the same way.
- **A Cloudflare Tunnel.** Lets a computer accept connections without opening a port. See [Cloudflare Tunnel](#cloudflare-tunnel) below.

Each computer listens on TCP port **41890** by default. You can change the address and port in **Settings → Mesh → Listen on**.

The computers don't all need to reach each other. If laptop A can't reach desktop B, but both can reach a home server C, Patchbay finds that out and sends A's traffic for B through C. It keeps trying the direct route and switches back as soon as it works. Settings → Mesh shows a relayed computer as "Connected · through C".

## Setting up a mesh

### Start a mesh on the first computer

1. Open **Settings → Mesh**.
2. Check the computer's name. Other computers show it under this name.
3. Under **Start a mesh**, choose a passphrase of at least 8 characters, enter it twice and click **Start mesh**.

The passphrase can be something you can remember. See [How the mesh stays private](#how-the-mesh-stays-private) for why that's safe.

### Add another computer

1. On the new computer, open **Settings → Mesh**.
2. Under **Join a mesh**, enter the address of a computer already in the mesh, then the passphrase, and click **Ask to join**. The address can be an IP address (`100.64.0.7`), a host name, an address with a port (`100.64.0.7:41890`) or a URL (`wss://mesh.example.com`).
3. The new computer shows a six-digit code.
4. The computer it connected to shows a join request with a code. Check that both codes match, then click **Approve**.

If the codes differ, click **Decline**: something sits between the two computers.

After approval, the new computer gets everything in the mesh, and its own chats and sessions join the mesh too. You need to enter an address only once. Computers tell each other the addresses they reached one another at, so the new computer finds the others on its own. Addresses that only work in one direction, such as a Cloudflare Tunnel hostname, go under **Public addresses**.

## Driving a session on another computer

Code sessions in the sidebar include those running on other computers. The session header shows which computer runs a session, with a dot for whether it's online.

Opening such a session connects you to its agent without interrupting it. Prompts, permission answers, model and mode changes and the Stop button all go to the computer running it. Every computer watching the session sees the same transcript, live, including prompts sent from other computers. A permission request appears on all of them, and the first answer counts.

The terminal panel opens a shell on the computer that runs the session, wherever you are.

## Moving a session

To move a session to another computer:

1. Click **Move to another computer** (the monitor icon) in the session header.
2. Choose the computer and the folder there.
3. Click **Move**.

A session moves only between turns: if the agent is working, wait for it to finish. Then:

- The agent stops on the old computer.
- The project's uncommitted and untracked files are copied to the new computer as git objects. Ignored files stay where they are. The old computer's branch, index and files aren't changed.
- On the new computer, the session continues in the folder you chose:
  - An existing checkout of the same repository is reused. It must not have uncommitted changes of its own.
  - A missing folder is created and filled from the old computer.
  - The branch moves to where it was on the old computer. If the new computer's copy of that branch has other commits, the session continues on a new branch named `<branch>-from-<computer>` instead, and Patchbay tells you.
- The agent's own session history moves too, so it remembers the conversation.

A folder that isn't a git repository is copied as a whole, up to 512 MB.

## When a computer goes offline

After every turn of a Code session, the other computers in the mesh fetch a **checkpoint**: the project's files as they are, and the agent's session history. Only what changed since the last checkpoint is transferred.

If the computer running a session goes offline, the session shows a banner on the others. Click **Take over here** and choose a folder. The session continues from the last checkpoint this computer received. At most the turn that was still running is lost.

Patchbay never takes a session over by itself. It can't tell a computer that has gone offline from one that is cut off from the rest of the network.

If the old computer comes back, it sees that the session moved and stops its agent. Any work it did after the checkpoint is kept in the project as a git ref named `refs/patchbay/orphaned/<session>-<time>`. Nothing is thrown away. To look at that work:

```bash
git for-each-ref refs/patchbay/orphaned
```

## Removing a computer

In **Settings → Mesh**, click **Remove** next to the computer. It is disconnected and gets no further changes.

Removing a computer also changes the key that encrypts synced secrets. However, the removed computer keeps the copies of chats, sessions and API keys it already has. If it might be in the wrong hands, replace those API keys with your providers.

To take the current computer out of the mesh, click **Leave mesh**. It stops syncing and forgets the passphrase, and keeps its own copy of everything.

## Cloudflare Tunnel

A Cloudflare Tunnel lets a computer accept mesh connections without a public IP address or an open port.

1. In your tunnel's configuration, route a hostname to Patchbay's listener:

   ```yaml
   ingress:
     - hostname: mesh.example.com
       service: http://localhost:41890
     - service: http_status:404
   ```

2. In **Settings → Mesh → Public addresses** on that computer, add `wss://mesh.example.com`.

Other computers can then join or connect through `wss://mesh.example.com` without any Cloudflare software of their own.

Patchbay can't sign in to Cloudflare Access, so don't protect this hostname with an Access policy. The mesh's passphrase and your approval of each computer guard it instead.

## How the mesh stays private

- **The passphrase never crosses the network.** Each connection proves both sides know it with SPAKE2, a password-authenticated key exchange. Someone who records a connection can't test passphrases against the recording. Each guess needs a live connection attempt, and Patchbay slows those down after every failure. That is why a passphrase you can remember is fine.
- **Everything is encrypted.** The SPAKE2 result keys a [Noise](https://noiseprotocol.org) session, which encrypts all traffic with forward secrecy. Traffic relayed through another computer is encrypted end to end, so the relaying computer can't read it.
- **The passphrase alone doesn't let a computer in.** A member must approve each new computer after comparing the codes. Approvals and removals are signed records that every computer checks.
- **Each computer has its own keys.** A computer that knows the passphrase still can't pose as a member, because each member's keys are fixed in its signed record.
- **Secrets are encrypted at rest.** Synced API keys and MCP secrets sit in each computer's database encrypted with a key only members have.

Every member is fully trusted. A member can drive Code sessions on every other member, and those agents run commands on that computer. Approve only computers you control.

Behind a Cloudflare Tunnel, every connection arrives from the tunnel's local address. The slow-down after failed attempts therefore applies to all tunnel traffic together.

## Troubleshooting

- **"The passphrase doesn't match this mesh's"**: the passphrase is wrong. Click **Back** and try again.
- **The join stays at "Connecting"**: the address isn't reachable. Check that the other computer runs Patchbay and is in a mesh, that the address and port are right, and that no firewall blocks TCP 41890.
- **A computer stays "Offline"**: no other computer knows a working address for it. On that computer, add one under **Public addresses**, or add another computer's address under **Add an address**.
- **"Can't listen on …"**: another program uses the port. Pick another port under **Listen on**.
- **Windows asks about the firewall** the first time Patchbay listens: allow it on the networks your other computers use (for example, private networks or the Tailscale adapter).
- **A move fails with "has uncommitted changes"**: the target folder has work of its own. Commit or stash it, or choose another folder.

## Limitations

- Moving the agent's own session history works with Grok Build, the agent Patchbay ships with. With other agents, the moved session starts fresh and gets the conversation so far as context on its first prompt.
- Window preferences (theme, text size, shortcuts and similar) stay per computer.
- Two copies of the same Patchbay database can't both join a mesh, because they share one identity. Use a fresh install on each computer instead of copying the data folder.

For the design behind all this, see [design/mesh.md](design/mesh.md).
