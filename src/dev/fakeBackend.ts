// Dev-only in-browser stand-in for the Rust backend, so the UI can be exercised in a plain
// browser (`pnpm dev`, then open http://localhost:1420). Never bundled into the Tauri build.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type { Channel } from "@tauri-apps/api/core";
import { emptyParams } from "../lib/ipc";
import type {
  MeshChatEvent,
  MeshStatus,
  McpServer,
  PermissionRule,
  Prompt,
  SearchHit,
  Attachment,
  ChatEvent,
  CodeEnvelope,
  CodeSession,
  Conversation,
  Message,
  Model,
  Provider,
  TermEvent,
  AcpConfigOption,
  AgentId,
  AgentInfo,
} from "../lib/ipc";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const fakeAgent = (id: AgentId, label: string, program: string | null, extra: Partial<AgentInfo> = {}): AgentInfo => ({
  id,
  label,
  available: program != null,
  program,
  source: program ? "path" : null,
  version: program ? "1.0.0" : null,
  error: program ? null : `${label} not found.`,
  installHint: `Install ${label}, then sign in from a terminal.`,
  usesPatchbayModels: id === "grok",
  supportsInstructions: id === "grok" || id === "claude-code",
  ...extra,
});
const FAKE_AGENTS: AgentInfo[] = [
  fakeAgent("grok", "Grok Build", "/opt/patchbay/grok", { source: "bundled" }),
  fakeAgent("claude-code", "Claude Code", "/usr/bin/npx", { source: "npx", version: null }),
  fakeAgent("opencode", "OpenCode", "/usr/bin/opencode"),
  fakeAgent("cursor", "Cursor CLI", "/usr/bin/cursor-agent"),
  fakeAgent("kilo", "Kilo Code", null),
];
/** Reasoning effort per fake session, for agents that report it as a config option. */
const fakeConfig: Record<string, string> = {};
const fakeEffort = (value: string): AcpConfigOption => ({
  id: "effort",
  name: "Effort",
  category: "thought_level",
  type: "select",
  currentValue: value,
  options: ["low", "medium", "high"].map((v) => ({ value: v, name: v[0].toUpperCase() + v.slice(1) })),
});
const DEMO_FILES = ["README.md", "Cargo.toml", "src/main.rs", "src/lib.rs", "src/code/acp.rs", "src/code/mcp.rs", "docs/guide.md", "package.json"];
const id = () => crypto.randomUUID();
const now = () => Date.now();

const db = {
  providers: [] as (Provider & { key?: string })[],
  models: [] as Model[],
  settings: {} as Record<string, string>,
  conversations: [] as Conversation[],
  messages: [] as Omit<Message, "siblingIds">[],
  attachments: [] as Attachment[],
  code: [] as CodeSession[],
  /** Message tree: what each conversation shows is the path to its active leaf. */
  leaf: {} as Record<string, string | null>,
  mcp: [] as McpServer[],
  rules: [] as PermissionRule[],
  prompts: [] as Prompt[],
  transcripts: {} as Record<string, string>,
};

type StoredMessage = Omit<Message, "siblingIds">;

function pathTo(convId: string, leafId: string | null): Message[] {
  const out: StoredMessage[] = [];
  let cur = leafId ? db.messages.find((m) => m.id === leafId) : undefined;
  while (cur) {
    out.unshift(cur);
    const parent: string | null = cur.parentId;
    cur = parent ? db.messages.find((m) => m.id === parent) : undefined;
  }
  return out.map((m) => ({
    ...m,
    siblingIds: db.messages
      .filter((x) => x.conversationId === convId && x.role === m.role && x.parentId === m.parentId)
      .sort((x, y) => x.createdAt - y.createdAt)
      .map((x) => x.id),
  }));
}

function latestLeaf(id: string): string {
  let cur = id;
  for (;;) {
    const kids = db.messages.filter((m) => m.parentId === cur).sort((x, y) => y.createdAt - x.createdAt);
    if (!kids.length) return cur;
    cur = kids[0].id;
  }
}

let clock = 0;
/** Strictly increasing timestamps so sibling order is stable. */
const tick = () => (clock = Math.max(now(), clock + 1));

const pending = new Map<string, (opt: string | null) => void>();
const cancelled = new Set<string>();

function send<T>(ch: unknown, msg: T) {
  (ch as Channel<T>).onmessage(msg);
}

function codeEmit(codeSessionId: string, event: CodeEnvelope["event"]) {
  emit("code://event", { codeSessionId, event } satisfies CodeEnvelope);
}

async function fakeChat(a: Record<string, unknown>, regenerate?: string) {
  const convId = a.conversationId as string;
  const ch = a.onEvent;
  const conv = db.conversations.find((c) => c.id === convId)!;
  const atts = ((a.attachmentIds as string[]) ?? []).map((x) => db.attachments.find((t) => t.id === x)!).filter(Boolean);
  const first = !db.messages.some((m) => m.conversationId === convId);
  let user: StoredMessage;
  if (regenerate) {
    user = db.messages.find((m) => m.id === regenerate)!;
  } else {
    const parentId = a.parentId === undefined ? (db.leaf[convId] ?? null) : (a.parentId as string) || null;
    user = {
      id: id(), conversationId: convId, role: "user", content: a.text as string, reasoning: null,
      modelRef: null, error: null, createdAt: tick(), attachments: atts, usage: null, parentId,
    };
    db.messages.push(user);
  }
  const asst: StoredMessage = {
    id: id(), conversationId: convId, role: "assistant", content: "", reasoning: null,
    modelRef: a.modelRef as string, error: null, createdAt: tick(), attachments: [], usage: null, parentId: user.id,
  };
  db.messages.push(asst);
  db.leaf[convId] = asst.id;
  const text = user.content;
  conv.modelRef = a.modelRef as string;
  conv.updatedAt = now();
  send<ChatEvent>(ch, { type: "started", userMessageId: user.id, assistantMessageId: asst.id });
  if (first) {
    conv.title = text.split("\n")[0].slice(0, 60) || atts[0]?.name || "New chat";
    send<ChatEvent>(ch, { type: "title", title: conv.title });
  }
  const reasoning = "Considering the request and attachments…";
  for (const w of reasoning.split(/(?<= )/)) {
    await sleep(15);
    asst.reasoning = (asst.reasoning ?? "") + w;
    send<ChatEvent>(ch, { type: "reasoning", text: w });
  }
  if (text.includes("fail")) {
    asst.error = "Endpoint returned 401: invalid API key (HTTP 401)";
    send<ChatEvent>(ch, { type: "error", message: asst.error });
    return;
  }
  const reply = [
    `**Echo:** ${text || "(no text)"}`,
    "",
    `- attachments: ${atts.map((x) => `${x.name} (${x.kind})`).join(", ") || "none"}`,
    "",
    "| col | value |\n| --- | --- |\n| a | 1 |",
    "",
    "```rust\nfn main() {\n    println!(\"hello from patchbay\");\n}\n```",
    "",
    "Math: inline $e^{i\\pi} + 1 = 0$, also \\(a^2 + b^2 = c^2\\), prices stay text: $5 and $10.",
    "",
    "\\[\n\\int_0^1 x^2\\,dx = \\tfrac{1}{3}\n\\]",
    "",
    "```mermaid\ngraph LR\n  A[Prompt] --> B{Model}\n  B --> C[Reply]\n```",
    "",
    "See [the docs](https://example.com/docs).",
  ].join("\n");
  cancelled.delete(convId);
  for (const w of reply.split(/(?<=\s)/)) {
    if (cancelled.has(convId)) {
      send<ChatEvent>(ch, { type: "done", stopReason: "cancelled", usage: null });
      return;
    }
    await sleep(20);
    asst.content += w;
    send<ChatEvent>(ch, { type: "text", text: w });
  }
  const prior = db.messages.filter((m) => m.conversationId === convId && m.usage).reduce((n, m) => n + m.usage!.input + m.usage!.output, 0);
  asst.usage = { input: prior + 1200 + text.length * 4, cachedRead: prior, cacheWrite: 0, output: Math.round(asst.content.length / 4), reasoning: 12, costUsd: null, apiMs: 800 };
  send<ChatEvent>(ch, { type: "done", stopReason: "EndTurn", usage: asst.usage });
  if (first) {
    // Stand-in for the generated title that follows the first reply.
    await sleep(400);
    conv.title = `Generated: ${conv.title}`.slice(0, 60);
    send<ChatEvent>(ch, { type: "title", title: conv.title });
  }
}

async function fakeAgentTurn(csid: string, text: string, mentions: string[] = []) {
  const s = db.code.find((c) => c.id === csid)!;
  const say = async (t: string) => {
    for (const w of t.split(/(?<=\s)/)) {
      await sleep(15);
      codeEmit(csid, { type: "update", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: w } } });
    }
  };
  codeEmit(csid, { type: "update", update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Planning the change." } } });
  codeEmit(csid, {
    type: "update",
    update: {
      sessionUpdate: "plan",
      entries: [
        { content: "Read README.md", priority: "high", status: "completed" },
        { content: "Edit README.md", priority: "high", status: "in_progress" },
        { content: "Run ls", priority: "medium", status: "pending" },
      ],
    },
  });
  await say(`Working on: ${text}\n`);
  if (mentions.length) await say(`\nMentioned files: ${mentions.map((m) => `\`${m}\``).join(", ")}\n`);
  const readId = id();
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: readId, title: "Read `README.md`", kind: "read", status: "completed", rawInput: { path: `${s.cwd}/README.md` }, content: [{ type: "content", content: { type: "text", text: "# demo\n\nOld intro." } }] } });
  const editId = id();
  const diff = { type: "diff" as const, path: `${s.cwd}/README.md`, oldText: "# demo\n\nOld intro.\n", newText: "# demo\n\nNew intro written by the agent.\n\n## Usage\n\nRun it.\n" };
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: editId, title: "Edit `README.md`", kind: "edit", status: "pending", rawInput: { file_path: "README.md" } } });
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, content: [diff] } });
  const reqId = id();
  const rule = db.rules.find((r) => r.toolKind === "edit" && (r.cwd === "*" || r.cwd === s.cwd));
  const choice = rule
    ? (codeEmit(csid, { type: "autoApproved", toolCallId: editId, description: "Auto-approved: all edit calls in this folder" }), "allow-once")
    : await new Promise<string | null>((resolve) => {
    pending.set(reqId, resolve);
    codeEmit(csid, {
      type: "permission",
      requestId: reqId,
      request: {
        sessionId: "acp",
        toolCall: { toolCallId: editId, title: "Edit `README.md`", kind: "edit" },
        options: [
          { optionId: "allow-edits-session", name: "Yes, allow all edits during this session", kind: "allow_always" },
          { optionId: "allow-once", name: "Yes", kind: "allow_once" },
          { optionId: "reject-once", name: "No", kind: "reject_once" },
        ],
      },
    });
  });
  if (!rule) codeEmit(csid, { type: "permissionResolved", requestId: reqId });
  if (!choice || choice.startsWith("reject")) {
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, status: "failed" } });
    await say("Okay, I won't make that edit.");
    codeEmit(csid, { type: "promptDone", stopReason: choice ? "end_turn" : "cancelled" });
    return;
  }
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, status: "completed" } });
  const runId = id();
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: runId, title: "Execute `ls`", kind: "execute", status: "in_progress", rawInput: { command: "ls" } } });
  await sleep(300);
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: runId, status: "completed", content: [{ type: "content", content: { type: "text", text: "README.md\nsrc\nCargo.toml" } }] } });
  const termId = id();
  codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: termId, title: "Execute `cargo check`", kind: "execute", status: "completed", rawInput: { command: "cargo check" }, content: [{ type: "terminal", terminalId: "term-1" }], rawOutput: { stdout: "Finished `dev` profile in 0.6s", exit_code: 0 } } });
  codeEmit(csid, { type: "update", update: { sessionUpdate: "plan", entries: [
    { content: "Read README.md", priority: "high", status: "completed" },
    { content: "Edit README.md", priority: "high", status: "completed" },
    { content: "Run ls", priority: "medium", status: "completed" },
  ] } });
  await say("Done. I updated the README intro and added a **Usage** section.");
  fakeUsage.input += 9000; fakeUsage.cachedRead += 6000; fakeUsage.output += 700;
  codeEmit(csid, { type: "usage", usage: { totals: { ...fakeUsage }, contextTokens: Math.min(118000, fakeUsage.input / 2), contextWindow: 128000, autoCompactPercent: 85 } });
  codeEmit(csid, { type: "promptDone", stopReason: "end_turn" });
}
const fakeUsage = { input: 0, cachedRead: 0, cacheWrite: 0, output: 0, reasoning: 0, costUsd: null as number | null, apiMs: 0 };

// ---- mesh: `?mesh=1` starts as a member of a four-computer mesh; otherwise not in one ----

const ME = "n-laptop";
const meshDemo = new URLSearchParams(location.search).has("mesh");
const mesh: MeshStatus = {
  nodeId: ME,
  name: "laptop",
  member: false,
  meshId: null,
  running: false,
  listen: "0.0.0.0:41890",
  port: null,
  advertise: [],
  joining: null,
  members: [],
  pending: [],
};

function meshMember() {
  Object.assign(mesh, {
    member: true,
    meshId: "demo-mesh",
    running: true,
    port: 41890,
    joining: null,
    members: [
      { nodeId: ME, name: mesh.name, state: "self", via: null, viaName: null, rttMs: null, url: null, admittedAt: now() - 9e8, approver: ME },
      { nodeId: "n-studio", name: "studio", state: "direct", via: null, viaName: null, rttMs: 4, url: "ws://100.64.0.7:41890", admittedAt: now() - 8e8, approver: ME },
      { nodeId: "n-nas", name: "nas", state: "relay", via: "n-studio", viaName: "studio", rttMs: null, url: null, admittedAt: now() - 7e8, approver: "n-studio" },
      { nodeId: "n-old", name: "old-desktop", state: "offline", via: null, viaName: null, rttMs: null, url: null, admittedAt: now() - 6e8, approver: ME },
    ],
  } satisfies Partial<MeshStatus>);
}

const meshStatusChanged = () => emit("mesh://status", null);

/** A reply streaming on "studio", shown live here. */
async function fakeRemoteStream() {
  const conv = db.conversations[0];
  const user: StoredMessage = {
    id: id(), conversationId: conv.id, role: "user", content: "What should I pack for three days in Lisbon?", reasoning: null,
    modelRef: null, error: null, createdAt: tick(), attachments: [], usage: null, parentId: db.leaf[conv.id] ?? null,
  };
  const asst: StoredMessage = {
    id: id(), conversationId: conv.id, role: "assistant", content: "", reasoning: null,
    modelRef: "m1", error: null, createdAt: tick(), attachments: [], usage: null, parentId: user.id,
  };
  db.messages.push(user, asst);
  db.leaf[conv.id] = asst.id;
  const ev = (event: MeshChatEvent["event"]) =>
    emit("mesh://chat", { conversationId: conv.id, streamer: "n-studio", streamerName: "studio", event } satisfies MeshChatEvent);
  ev({ type: "started", userMessageId: user.id, assistantMessageId: asst.id });
  emit("mesh://changed", { tables: ["messages"] });
  const reply = "Light layers, comfortable shoes for the hills, a compact umbrella and a power adapter (type F).";
  for (const w of reply.split(/(?<=\s)/)) {
    await sleep(120);
    asst.content += w;
    ev({ type: "text", text: w });
  }
  ev({ type: "done", stopReason: "EndTurn", usage: null });
}

export function install() {
  mockWindows("main");
  // Seed one provider + model so the UI starts usable.
  const p: Provider = { id: "p1", name: "Demo provider", kind: "openai_chat", baseUrl: "http://localhost:8765/v1", hasKey: true };
  db.providers.push(p);
  db.models.push({ id: "m1", providerId: "p1", modelId: "demo-model", displayName: "Demo model", vision: true, contextWindow: 128000, params: { ...emptyParams(), pricing: { input: 3, cachedInput: 0.3, cacheWrite: 3.75, output: 15 } } });
  db.models.push({ id: "m2", providerId: "p1", modelId: "demo-mini", displayName: "Demo mini", vision: false, contextWindow: 32000, params: emptyParams() });
  const seed = (cwd: string, title: string, ago: number, activity: CodeSession["activity"], hostNode = ME) =>
    db.code.push({ id: id(), cwd, acpSessionId: null, title, modelRef: "m1", pinned: false, createdAt: now() - ago, updatedAt: now() - ago, activity, hostNode, epoch: 0, agent: "grok", agentModel: null });
  seed("/home/demo/Projects/nulamp", "Windows installer testing", 1000, "edit");
  seed("/home/demo/Projects/nulamp", "Release to staging", 5000, "pr");
  seed("/home/demo/Projects/omnimark", "Interactive document controls", 3000, "branch", meshDemo ? "n-studio" : ME);
  seed("/home/demo/Work/omnimark", "Session overview", 9000, null, meshDemo ? "n-old" : ME);
  if (meshDemo) {
    meshMember();
    mesh.pending = [{ nodeId: "n-new", name: "travel-pc", code: "482 913" }];
    setTimeout(fakeRemoteStream, 2500);
  }
  const conv = (title: string, agoDays: number, pinned = false) =>
    db.conversations.push({ id: id(), title, modelRef: "m1", systemPrompt: null, pinned, createdAt: now() - agoDays * 86_400_000, updatedAt: now() - agoDays * 86_400_000 });
  conv("Trip planning", 0.01);
  conv("Regex for log lines", 1.2, true);
  conv("Rust lifetimes", 3);
  conv("Quarterly report outline", 12);
  conv("Old experiment", 60);

  mockIPC(
    async (cmd, raw) => {
      const a = (raw ?? {}) as Record<string, unknown>;
      switch (cmd) {
        case "list_providers": return db.providers.map(({ key: _k, ...x }) => x);
        case "save_provider": {
          const i = a.input as { id?: string; name: string; kind: Provider["kind"]; baseUrl: string; apiKey?: string };
          let x = db.providers.find((q) => q.id === i.id);
          if (!x) { x = { id: id(), name: "", kind: i.kind, baseUrl: "", hasKey: false }; db.providers.push(x); }
          Object.assign(x, { name: i.name, kind: i.kind, baseUrl: i.baseUrl });
          if (i.apiKey !== undefined) x.hasKey = i.apiKey !== "";
          return { ...x };
        }
        case "delete_provider":
          db.providers = db.providers.filter((x) => x.id !== a.id);
          db.models = db.models.filter((m) => m.providerId !== a.id);
          return null;
        case "discover_models":
          await sleep(300);
          return [["gpt-demo-mini", 128000], ["gpt-demo-large", 400000], ["claude-demo", null]].map(([x, c]) => ({ id: x as string, displayName: x as string, contextWindow: c as number | null }));
        case "list_models": return db.models;
        case "save_model": {
          const i = a.input as Partial<Model> & { providerId: string; modelId: string };
          let m = db.models.find((x) => x.id === i.id || (x.providerId === i.providerId && x.modelId === i.modelId));
          if (!m) { m = { id: id(), providerId: i.providerId, modelId: i.modelId, displayName: i.modelId, vision: true, contextWindow: null, params: emptyParams() }; db.models.push(m); }
          Object.assign(m, { modelId: i.modelId, displayName: i.displayName || m.displayName, vision: i.vision ?? m.vision, contextWindow: i.contextWindow !== undefined ? i.contextWindow : m.contextWindow, params: i.params ?? m.params });
          return m;
        }
        case "delete_model": db.models = db.models.filter((m) => m.id !== a.id); return null;
        case "get_preferences": return JSON.parse(db.settings.preferences ?? "{}");
        case "set_preferences": {
          const p = a.prefs as { terminalShell?: string; autoCompactPercent?: number | null };
          if (p.terminalShell && !p.terminalShell.startsWith("/")) throw `terminal shell ${p.terminalShell} does not exist`;
          if (p.autoCompactPercent != null && (p.autoCompactPercent < 10 || p.autoCompactPercent > 99)) throw "auto-compact threshold must be between 10 and 99%";
          db.settings.preferences = JSON.stringify(a.prefs);
          return null;
        }
        case "get_setting": return db.settings[a.key as string] ?? null;
        case "set_setting": db.settings[a.key as string] = a.value as string; return null;

        case "list_conversations": return [...db.conversations].sort((x, y) => y.updatedAt - x.updatedAt);
        case "create_conversation": {
          const c: Conversation = { id: id(), title: "New chat", modelRef: (a.modelRef as string) ?? null, systemPrompt: null, pinned: false, createdAt: now(), updatedAt: now() };
          db.conversations.push(c);
          return c;
        }
        case "update_conversation": {
          const c = db.conversations.find((x) => x.id === a.id)!;
          if (a.title != null) c.title = a.title as string;
          if (a.modelRef != null) c.modelRef = a.modelRef as string;
          if (a.systemPrompt != null) c.systemPrompt = (a.systemPrompt as string) || null;
          if (a.pinned != null) c.pinned = a.pinned as boolean;
          return null;
        }
        case "delete_conversation":
          delete db.leaf[a.id as string];
          db.conversations = db.conversations.filter((x) => x.id !== a.id);
          db.messages = db.messages.filter((m) => m.conversationId !== a.id);
          return null;
        case "list_messages": return pathTo(a.conversationId as string, db.leaf[a.conversationId as string] ?? null);
        case "switch_branch": db.leaf[a.conversationId as string] = latestLeaf(a.messageId as string); return null;
        case "search_messages": {
          const q = String(a.query).toLowerCase().trim();
          if (q.length < 2) return [];
          return db.messages
            .filter((m) => m.content.toLowerCase().includes(q))
            .slice(-30)
            .reverse()
            .map((m): SearchHit => {
              const i = m.content.toLowerCase().indexOf(q);
              const snip = m.content.slice(Math.max(0, i - 30), i) + "\u0001" + m.content.slice(i, i + q.length) + "\u0002" + m.content.slice(i + q.length, i + q.length + 40);
              return { conversationId: m.conversationId, conversationTitle: db.conversations.find((c) => c.id === m.conversationId)?.title ?? "", messageId: m.id, role: m.role, snippet: snip, createdAt: m.createdAt };
            });
        }
        case "send_chat_message": fakeChat(a); return null;
        case "regenerate_reply": fakeChat(a, a.userMessageId as string); return null;
        case "cancel_chat": cancelled.add(a.conversationId as string); return null;

        case "add_attachment_path":
        case "add_attachment_bytes": {
          const name = cmd === "add_attachment_path" ? String(a.path).split("/").pop()! : (a.name as string);
          const ext = name.split(".").pop()!.toLowerCase();
          const kind = ["png", "jpg", "jpeg", "gif", "webp"].includes(ext) ? "image" : ext === "pdf" ? "pdf" : "text";
          if (["zip", "exe", "bin"].includes(ext)) throw `unsupported binary file: ${name}`;
          const att: Attachment = { id: id(), sha256: "x", name, mime: kind === "image" ? `image/${ext}` : kind === "pdf" ? "application/pdf" : "text/plain", kind, size: 12345, path: `/fake/${name}` };
          db.attachments.push(att);
          return att;
        }
        case "attachment_thumbnail": {
          const c = document.createElement("canvas");
          c.width = c.height = 64;
          const g = c.getContext("2d")!;
          g.fillStyle = "#e5534b"; g.fillRect(0, 0, 64, 64);
          g.fillStyle = "#fff"; g.fillRect(16, 16, 32, 32);
          return c.toDataURL();
        }

        case "plugin:dialog|save": {
          const o = a.options as { defaultPath?: string } | undefined;
          return `/home/demo/Downloads/${o?.defaultPath ?? "export.md"}`;
        }
        case "write_text_file":
          console.info("fake backend: write", a.path, String(a.contents).length, "chars");
          (window as unknown as Record<string, unknown>).__lastExport = { path: a.path, contents: a.contents };
          return null;
        case "plugin:opener|open_url":
          console.info("fake backend: open url", a.url);
          (window as unknown as Record<string, unknown>).__lastOpenedUrl = a.url;
          return null;

        case "plugin:dialog|open": {
          const o = a.options as { directory?: boolean } | undefined;
          return o?.directory ? "/home/demo/projects/demo-app" : ["/home/demo/photo.png", "/home/demo/report.pdf", "/home/demo/notes.md"];
        }

        case "list_code_sessions": return [...db.code].sort((x, y) => y.updatedAt - x.updatedAt);
        case "create_code_session": {
          const s: CodeSession = { id: id(), cwd: a.cwd as string, acpSessionId: null, title: "New session", modelRef: (a.modelRef as string) ?? null, pinned: false, createdAt: now(), updatedAt: now(), activity: null, agent: ((a.agent as AgentId | null) ?? (JSON.parse(db.settings.preferences ?? "{}").defaultAgent as AgentId | undefined) ?? "grok"), agentModel: null, hostNode: ME, epoch: 0 };
          db.code.push(s);
          return s;
        }
        case "update_code_session": {
          const s = db.code.find((x) => x.id === a.id)!;
          if (a.title != null) s.title = a.title as string;
          if (a.pinned != null) s.pinned = a.pinned as boolean;
          if (a.activity != null) s.activity = a.activity as CodeSession["activity"];
          return null;
        }
        case "delete_code_session": db.code = db.code.filter((x) => x.id !== a.id); return null;
        case "code_is_running": return false;
        case "code_start": {
          const csid = a.id as string;
          setTimeout(() => {
            codeEmit(csid, { type: "status", state: "starting", message: null });
            setTimeout(() => {
              const grok = (db.code.find((c) => c.id === csid)?.agent ?? "grok") === "grok";
              codeEmit(csid, {
                type: "sessionReady", acpSessionId: "acp-" + csid, loaded: false,
                capabilities: { loadSession: true, promptCapabilities: { image: false, embeddedContext: true }, mcpCapabilities: { http: true, sse: true } },
                agentInfo: { name: "fake-agent" },
                models: grok
                  ? { currentModelId: "pb-m1", availableModels: db.models.map((m) => ({ modelId: "pb-" + m.id, name: m.displayName })) }
                  : { currentModelId: "sonnet", availableModels: [{ modelId: "sonnet", name: "Sonnet" }, { modelId: "opus", name: "Opus" }] },
                modes: grok
                  ? null
                  : { currentModeId: "build", availableModes: [{ id: "build", name: "Build" }, { id: "plan", name: "Plan" }] },
                configOptions: grok ? [] : [fakeEffort(fakeConfig[csid] ?? "medium")],
                extensions: { statusLine: grok, rewind: grok },
              });
              codeEmit(csid, {
                type: "update",
                update: {
                  sessionUpdate: "available_commands_update",
                  availableCommands: [
                    { name: "compact", description: "Summarize the conversation to free up context" },
                    { name: "context", description: "Show context window usage" },
                    { name: "model", description: "Switch model", input: { hint: "<model>" } },
                    { name: "review", description: "Review the current changes", input: { hint: "[focus]" } },
                  ],
                },
              });
              codeEmit(csid, { type: "status", state: "ready", message: null });
            }, 400);
          }, 10);
          return null;
        }
        case "code_stop": return null;
        case "code_prompt": fakeAgentTurn(a.id as string, a.text as string, (a.mentions as string[]) ?? []); return null;
        case "get_code_transcript": return db.transcripts[a.id as string] ?? null;
        case "save_code_transcript": db.transcripts[a.id as string] = a.json as string; return null;
        case "list_project_files": await sleep(80); return DEMO_FILES;
        case "list_mcp_servers": return db.mcp.map((m) => ({ ...m, config: { ...m.config, env: m.config.env.map((kv) => (kv.secret ? { ...kv, value: "", stored: !!kv.value } : kv)) } }));
        case "save_mcp_server": {
          const i = a.input as Omit<McpServer, "id"> & { id?: string };
          if (!i.name.trim()) throw "name is required";
          if (i.config.transport === "stdio" && !i.config.command.trim()) throw "a stdio server needs a command";
          const sv: McpServer = { id: i.id ?? id(), name: i.name, enabled: i.enabled, config: i.config };
          db.mcp = [...db.mcp.filter((m) => m.id !== sv.id), sv];
          return sv;
        }
        case "delete_mcp_server": db.mcp = db.mcp.filter((m) => m.id !== a.id); return null;
        case "list_permission_rules": return db.rules;
        case "add_permission_rule": {
          const rule: PermissionRule = { id: id(), cwd: a.cwd as string, toolKind: a.toolKind as string, pattern: a.pattern as string, createdAt: now() };
          db.rules.push(rule);
          return rule;
        }
        case "delete_permission_rule": db.rules = db.rules.filter((r) => r.id !== a.id); return null;
        case "list_prompts": return [...db.prompts].sort((x, y) => x.name.localeCompare(y.name));
        case "save_prompt": {
          const existing = db.prompts.find((p) => p.id === a.id);
          const p: Prompt = { id: (a.id as string) ?? id(), name: a.name as string, content: a.content as string, createdAt: existing?.createdAt ?? now(), updatedAt: now() };
          db.prompts = [...db.prompts.filter((x) => x.id !== p.id), p];
          return p;
        }
        case "delete_prompt": db.prompts = db.prompts.filter((p) => p.id !== a.id); return null;
        case "usage_report": {
          const days = new Map<string, { day: string; modelRef: string | null; replies: number; usage: Message["usage"] }>();
          for (const m of db.messages) {
            if (m.role !== "assistant" || !m.usage) continue;
            const d = new Date(m.createdAt);
            const day = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
            const key = `${day}|${m.modelRef}`;
            const row = days.get(key) ?? { day, modelRef: m.modelRef, replies: 0, usage: { input: 0, cachedRead: 0, cacheWrite: 0, output: 0, reasoning: 0, costUsd: null, apiMs: 0 } };
            row.replies++;
            row.usage = { ...row.usage!, input: row.usage!.input + m.usage.input, output: row.usage!.output + m.usage.output, cachedRead: row.usage!.cachedRead + m.usage.cachedRead };
            days.set(key, row);
          }
          // A little history so the chart has shape.
          for (let i = 1; i < Math.min(Number(a.days), 30); i += 2) {
            const d = new Date(Date.now() - i * 86_400_000);
            const day = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
            days.set(`${day}|m1`, { day, modelRef: "m1", replies: 3 + (i % 5), usage: { input: 20000 + i * 3000, cachedRead: 5000, cacheWrite: 0, output: 4000 + i * 500, reasoning: 0, costUsd: null, apiMs: 0 } });
          }
          return {
            chat: [...days.values()],
            code: db.code.slice(0, 2).map((s) => ({ id: s.id, title: s.title, cwd: s.cwd, modelRef: "m1", updatedAt: s.updatedAt, usage: { ...fakeUsage, input: 90000, output: 7000 } })),
          };
        }
        case "quick_submit": case "quick_dismiss": return null;
        // No native webview in the browser: approximate page zoom with CSS.
        case "plugin:webview|set_webview_zoom": document.documentElement.style.zoom = String(a.value); return null;
        case "plugin:notification|is_permission_granted": return true;
        case "plugin:notification|notify": console.info("fake backend: notification", a); return null;
        case "code_cancel":
          for (const [k, r] of pending) { r(null); pending.delete(k); }
          return null;
        case "code_set_model": case "code_set_mode": return null;
        case "code_set_config_option": {
          fakeConfig[a.id as string] = a.value as string;
          codeEmit(a.id as string, { type: "config", models: null, modes: null, configOptions: [fakeEffort(a.value as string)] });
          return null;
        }
        case "list_agents": return FAKE_AGENTS;
        case "code_rewind": await sleep(150); return null;
        case "code_usage": return { totals: { ...fakeUsage }, contextTokens: null, contextWindow: 128000, autoCompactPercent: 85 };
        case "code_permission_respond": {
          const r = pending.get(a.requestId as string);
          pending.delete(a.requestId as string);
          r?.((a.optionId as string) ?? null);
          return null;
        }

        case "term_open": {
          setTimeout(() => send<TermEvent>(a.onEvent, { type: "data", data: btoa(`demo@patchbay:${a.cwd}$ `) }), 50);
          (window as unknown as Record<string, unknown>).__term = a.onEvent;
          return null;
        }
        case "term_write": {
          const d = (a.data as string).replace(/\r/g, "\r\n");
          send<TermEvent>((window as unknown as Record<string, unknown>).__term, { type: "data", data: btoa(d) });
          return null;
        }
        case "term_resize": case "term_close": return null;

        case "code_attach": {
          const s = db.code.find((x) => x.id === a.id);
          if (s?.hostNode === "n-old") return { running: false, busy: false, events: [], offline: true };
          setTimeout(() => codeEmit(a.id as string, { type: "status", state: "ready", message: null }), 50);
          return { running: false, busy: false, events: [] };
        }
        case "code_move": {
          await sleep(1200);
          const s = db.code.find((x) => x.id === a.id)!;
          s.hostNode = a.target as string;
          s.cwd = a.cwd as string;
          s.epoch += 1;
          return null;
        }
        case "code_take_over": {
          await sleep(800);
          const s = db.code.find((x) => x.id === a.id)!;
          Object.assign(s, { hostNode: ME, cwd: a.cwd as string, epoch: s.epoch + 1 });
          return null;
        }
        case "code_checkpoints": {
          const s = db.code.find((x) => x.hostNode === "n-old");
          return s ? { [s.id]: now() - 3_600_000 } : {};
        }
        case "mesh_status": return mesh;
        case "mesh_create": meshMember(); mesh.name = (a.name as string) || mesh.name; mesh.members[0].name = mesh.name; mesh.members = mesh.members.slice(0, 1); meshStatusChanged(); return null;
        case "mesh_join": {
          mesh.joining = { url: a.url as string, code: null, status: "connecting", error: null };
          setTimeout(() => { mesh.joining = { ...mesh.joining!, status: "waiting", code: "305 118" }; meshStatusChanged(); }, 900);
          return null;
        }
        case "mesh_cancel_join": mesh.joining = null; return null;
        case "mesh_approve": case "mesh_reject": {
          const p = mesh.pending.find((x) => x.nodeId === a.nodeId);
          mesh.pending = mesh.pending.filter((x) => x.nodeId !== a.nodeId);
          if (p && cmd === "mesh_approve") {
            mesh.members.push({ nodeId: p.nodeId, name: p.name, state: "direct", via: null, viaName: null, rttMs: 31, url: null, admittedAt: now(), approver: ME });
          }
          return null;
        }
        case "mesh_remove": {
          const m = mesh.members.find((x) => x.nodeId === a.nodeId);
          if (m) m.state = "removed";
          return null;
        }
        case "mesh_leave": Object.assign(mesh, { member: false, meshId: null, running: false, members: [], pending: [] }); return null;
        case "mesh_set_config": {
          if (a.name != null) mesh.name = a.name as string;
          if (a.listen != null) mesh.listen = (a.listen as string) || "0.0.0.0:41890";
          if (a.advertise != null) mesh.advertise = a.advertise as string[];
          return null;
        }
        case "mesh_add_address": return null;
        case "mesh_chat_streams": return [];
      }
      console.warn("fake backend: unhandled", cmd, a);
      return null;
    },
    { shouldMockEvents: true },
  );
}
