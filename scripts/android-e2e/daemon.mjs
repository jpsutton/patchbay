#!/usr/bin/env node
// Drives a running patchbayd (the desktop-side node of scripts/android-e2e.sh) through the same
// WebSocket API its browser UI uses: creates a mesh, seeds something for the phone to show, and
// approves the phone's request to join.
//
//   daemon.mjs seed    --ws URL --token-file F --name N --listen ADDR --mock-url URL --project DIR [--agent patchbay]
//   daemon.mjs approve --ws URL --token-file F --phone NAME [--timeout SECS]
//   daemon.mjs status  --ws URL --token-file F
//
// The mesh passphrase comes from the environment (E2E_PASSPHRASE), not the command line, so it
// doesn't show up in `ps`. Needs Node 22 or newer (a global WebSocket).

import { readFileSync } from "node:fs";

export const CHAT_TITLE = "E2E chat from the desktop";
export const SESSION_TITLE = "E2E try !edit from the phone";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function flags(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) throw new Error(`unexpected argument ${a}`);
    out[a.slice(2)] = argv[++i];
  }
  return out;
}

function need(f, ...names) {
  for (const n of names) if (!f[n]) throw new Error(`--${n} is required`);
}

class Client {
  #ws;
  #next = 1;
  #pending = new Map();
  #channels = new Map();

  static async open(url, token) {
    const c = new Client();
    c.#ws = new WebSocket(url);
    await new Promise((resolve, reject) => {
      c.#ws.addEventListener("error", () => reject(new Error(`cannot reach ${url}`)), { once: true });
      c.#ws.addEventListener("open", resolve, { once: true });
    });
    const ready = new Promise((resolve, reject) => {
      c.#ws.addEventListener("message", (ev) => {
        const m = JSON.parse(ev.data);
        if (m.authError) reject(new Error(m.authError));
        else if (m.ready) resolve();
        else c.#dispatch(m);
      });
      setTimeout(() => reject(new Error("the daemon never said ready")), 10_000);
    });
    c.#ws.send(JSON.stringify({ auth: token }));
    await ready;
    return c;
  }

  #dispatch(m) {
    if (m.id != null && this.#pending.has(m.id)) {
      const p = this.#pending.get(m.id);
      this.#pending.delete(m.id);
      if (m.err !== undefined) p.reject(new Error(m.err));
      else p.resolve(m.ok);
    } else if (m.chan != null) this.#channels.get(m.chan)?.(m.msg);
  }

  call(cmd, args = {}) {
    const id = this.#next++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#ws.send(JSON.stringify({ id, cmd, args }));
    });
  }

  /** A channel argument: `onMessage` gets what the daemon streams back. */
  channel(onMessage) {
    const n = this.#next++;
    this.#channels.set(n, onMessage);
    return `__CHANNEL__:${n}`;
  }

  close() {
    this.#ws.close();
  }
}

async function connect(f) {
  need(f, "ws", "token-file");
  return Client.open(f.ws, readFileSync(f["token-file"], "utf8").trim());
}

async function ensureMesh(c, f) {
  need(f, "name", "listen");
  const pass = process.env.E2E_PASSPHRASE;
  if (!pass) throw new Error("E2E_PASSPHRASE is not set");
  let st = await c.call("mesh_status");
  if (!st.member) {
    await c.call("mesh_set_config", { name: f.name, listen: f.listen });
    await c.call("mesh_create", { passphrase: pass, name: f.name });
    st = await c.call("mesh_status");
  }
  return st;
}

async function ensureModels(c, f) {
  need(f, "mock-url");
  const providers = await c.call("list_providers");
  let provider = providers.find((p) => p.name === "E2E mock");
  if (!provider || provider.baseUrl !== f["mock-url"]) {
    provider = await c.call("save_provider", {
      input: { id: provider?.id, name: "E2E mock", kind: "openai_chat", baseUrl: f["mock-url"], apiKey: "mock-key" },
    });
  }
  const models = await c.call("list_models");
  const model = async (modelId) =>
    models.find((m) => m.providerId === provider.id && m.modelId === modelId) ??
    (await c.call("save_model", {
      input: { providerId: provider.id, modelId, displayName: modelId, vision: true, contextWindow: 128000 },
    }));
  const echo = await model("mock-echo");
  const agent = await model("mock-agent");
  await c.call("set_setting", { key: "default_model", value: echo.id });
  return { echo, agent };
}

/** One chat with a reply, so the phone has something under Chats. */
async function ensureChat(c, echo) {
  const have = (await c.call("list_conversations")).find((x) => x.title === CHAT_TITLE);
  if (have) return have;
  const conv = await c.call("create_conversation", { modelRef: echo.id });
  const finished = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("the mock model never finished the reply")), 30_000);
    const onEvent = c.channel((ev) => {
      if (ev.type === "done") (clearTimeout(timer), resolve());
      if (ev.type === "error") (clearTimeout(timer), reject(new Error(`chat failed: ${ev.message}`)));
    });
    c.call("send_chat_message", {
      conversationId: conv.id,
      modelRef: echo.id,
      text: "Is this thing on?",
      attachmentIds: [],
      onEvent,
    }).catch(reject);
  });
  await finished;
  // The reply may bring its own generated title; ours goes on last.
  await sleep(1500);
  await c.call("update_conversation", { id: conv.id, title: CHAT_TITLE });
  return { ...conv, title: CHAT_TITLE };
}

async function ensureSession(c, f, agentModel) {
  need(f, "project");
  const agent = f.agent ?? "patchbay";
  const have = (await c.call("list_code_sessions")).find((s) => s.title === SESSION_TITLE);
  if (have) return have;
  const s = await c.call("create_code_session", { cwd: f.project, modelRef: agentModel.id, agent });
  await c.call("update_code_session", { id: s.id, title: SESSION_TITLE });
  return { ...s, title: SESSION_TITLE };
}

async function seed(f) {
  const c = await connect(f);
  try {
    const mesh = await ensureMesh(c, f);
    const { echo, agent } = await ensureModels(c, f);
    const chat = await ensureChat(c, echo);
    const session = await ensureSession(c, f, agent);
    const agents = await c.call("list_agents", {}).catch(() => []);
    const wanted = agents.find((a) => a.id === (f.agent ?? "patchbay"));
    console.log(
      JSON.stringify({
        nodeId: mesh.nodeId,
        meshName: f.name,
        chat: { id: chat.id, title: CHAT_TITLE },
        session: { id: session.id, title: SESSION_TITLE, agent: f.agent ?? "patchbay", agentAvailable: wanted?.available ?? null },
      }),
    );
  } finally {
    c.close();
  }
}

/** Approves the phone named `phone`, and only a phone with that name, until it is connected. */
async function approve(f) {
  need(f, "phone");
  const c = await connect(f);
  const deadline = Date.now() + Number(f.timeout ?? 90) * 1000;
  const refused = new Set();
  try {
    for (;;) {
      const st = await c.call("mesh_status");
      for (const p of st.pending ?? []) {
        if (p.name === f.phone && p.role === "remote") {
          console.error(`approving ${p.name} (code ${p.code})`);
          await c.call("mesh_approve", { nodeId: p.nodeId });
        } else if (!refused.has(p.nodeId)) {
          refused.add(p.nodeId);
          console.error(`ignoring a request from ${p.name} (${p.role}); only the phone "${f.phone}" is approved here`);
        }
      }
      const m = (st.members ?? []).find(
        (x) => x.name === f.phone && x.role === "remote" && x.state !== "offline" && x.state !== "removed",
      );
      if (m) {
        console.log(JSON.stringify({ phone: m.name, nodeId: m.nodeId, state: m.state }));
        return;
      }
      if (Date.now() > deadline) throw new Error(`${f.phone} did not join within ${f.timeout ?? 90}s`);
      await sleep(300);
    }
  } finally {
    c.close();
  }
}

async function status(f) {
  const c = await connect(f);
  try {
    const st = await c.call("mesh_status");
    console.log(
      JSON.stringify({
        member: st.member,
        nodeId: st.nodeId,
        name: st.name,
        port: st.port,
        members: (st.members ?? []).map((m) => ({ name: m.name, role: m.role ?? "computer", state: m.state })),
        pending: (st.pending ?? []).map((p) => ({ name: p.name, role: p.role })),
      }),
    );
  } finally {
    c.close();
  }
}

const [cmd, ...rest] = process.argv.slice(2);
const commands = { seed, approve, status };
if (!commands[cmd]) {
  console.error("usage: daemon.mjs seed|approve|status --ws URL --token-file FILE ...");
  process.exit(2);
}
commands[cmd](flags(rest)).catch((e) => {
  console.error(`daemon.mjs ${cmd}: ${e.message}`);
  process.exit(1);
});
