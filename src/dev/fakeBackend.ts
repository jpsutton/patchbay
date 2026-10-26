// Dev-only in-browser stand-in for the Rust backend, so the UI can be exercised in a plain
// browser (`pnpm dev`, then open http://localhost:1420). Never bundled into the Tauri build.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type { Channel } from "@tauri-apps/api/core";
import { emptyParams, usesPatchbayModels } from "../lib/ipc";
import type {
  MeshChatEvent,
  MeshStatus,
  McpServer,
  PermissionRule,
  PromptDelivery,
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
  AuthChoice,
  WireFrame,
  WireLogStatus,
} from "../lib/ipc";

/** The fake plan windows' anchor, so history samples keep matching them across calls. */
const fakePlanBase = Math.floor(Date.now() / 1000);
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
  usesPatchbayModels: id === "patchbay" || id === "grok",
  supportsInstructions: id === "patchbay" || id === "grok" || id === "claude-code" || id === "codex" || id === "antigravity",
  takesProviders: id === "claude-code" || id === "opencode" || id === "kilo",
  ...extra,
});
/** Sessions whose fake agent was started (code_start) and still runs. */
const fakeRunning = new Set<string>();

/** Sent when a chat message contains "wide table": many columns of prose, to check table layout. */
const WIDE_TABLE = [
  "Findings:",
  "",
  "| # | Check | Verdict | Fix on main | Catch |",
  "| --- | --- | --- | --- | --- |",
  "| 1 | Backups geo-redundant | Code, destructive | `modules/app-postgres/main.tf:22` hardcodes `false`. Add a variable and set it per caller. | ForceNew, so the server is recreated. Needs dump/restore and a manual Entra role re-map. Combine with #2. |",
  "| 2 | DBs in cluster region | Code, blocked | `*_postgres_location` in `prod.tfvars:33,42,79,100` | Rebuild. The tfvars comment says eastus is restricted for this subscription. No sign of a lifted restriction. Needs a support ticket. |",
].join("\n");

const FAKE_AGENTS: AgentInfo[] = [
  fakeAgent("patchbay", "Patchbay", "/opt/patchbay", { source: "builtin" }),
  fakeAgent("grok", "Grok Build", "/usr/local/bin/grok", { source: "path" }),
  fakeAgent("claude-code", "Claude Code", "/usr/bin/npx", { source: "npx", version: null }),
  fakeAgent("opencode", "OpenCode", "/usr/bin/opencode"),
  fakeAgent("cursor", "Cursor CLI", "/usr/bin/cursor-agent"),
  fakeAgent("kilo", "Kilo Code", null),
  fakeAgent("codex", "Codex", "/usr/bin/npx", { source: "npx", version: null }),
  fakeAgent("antigravity", "Antigravity", null),
];
/** Config option values per fake session, for agents that report them (effort, Fast mode, persona). */
const fakeConfig: Record<string, Record<string, string>> = {};
const fakeEffort = (value: string): AcpConfigOption => ({
  id: "effort",
  name: "Effort",
  category: "thought_level",
  type: "select",
  currentValue: value,
  options: ["low", "medium", "high"].map((v) => ({ value: v, name: v[0].toUpperCase() + v.slice(1) })),
});
/** Claude Code's other options, as its adapter sends them to a client without boolean options. */
const fakeOptions = (csid: string): AcpConfigOption[] => {
  const v = fakeConfig[csid] ?? {};
  return [
    fakeEffort(v.effort ?? "medium"),
    {
      id: "fast", name: "Fast mode", description: "Faster responses on supported models", category: "model_config",
      type: "select", currentValue: v.fast ?? "off",
      options: [{ value: "on", name: "On" }, { value: "off", name: "Off" }],
    },
    {
      id: "agent", name: "Agent", description: "Main-thread agent persona", type: "select", currentValue: v.agent ?? "default",
      options: [{ value: "default", name: "Default", description: "Standard Claude Code agent" }, { value: "reviewer", name: "reviewer" }],
    },
  ];
};
/** Patchbay's own agent's options: thinking effort (defaulting to the model's) and compact prompts. */
const fakePatchbayOptions = (csid: string): AcpConfigOption[] => {
  const v = fakeConfig[csid] ?? {};
  const efforts = [["none", "Off"], ["minimal", "Minimal"], ["low", "Low"], ["medium", "Medium"], ["high", "High"], ["xhigh", "Extra high"], ["max", "Max"]];
  return [
    {
      id: "effort", name: "Thinking", category: "thought_level", type: "select", currentValue: v.effort ?? "default",
      description: "How hard the model reasons before it answers. Models without reasoning ignore it.",
      options: [{ value: "default", name: "Model default (High)" }, ...efforts.map(([value, name]) => ({ value, name }))],
    },
    {
      id: "minify_prompts", name: "Compact prompts", type: "select", currentValue: v.minify_prompts ?? "off",
      description: "Send the agent's own prompts in a shortened form to save input tokens, and ask it to reply tersely, like caveman.",
      options: [{ value: "off", name: "Off", description: "Full prompts" }, { value: "on", name: "On", description: "Terse prompts and terse replies" }],
    },
  ];
};
const fakeOptionsFor = (csid: string): AcpConfigOption[] => {
  const agent = db.code.find((c) => c.id === csid)?.agent ?? "patchbay";
  return agent === "patchbay" ? fakePatchbayOptions(csid) : agent === "grok" ? [] : fakeOptions(csid);
};
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

/**
 * Agents that want a sign-in first (as Codex and Antigravity do when signed out).
 * Claude Code's methods are terminal ones: its login command runs in a terminal.
 */
const FAKE_AUTH: Partial<Record<AgentId, AuthChoice[]>> = {
  "claude-code": [
    { id: "claude-ai-login", name: "Claude Subscription", description: "Use Claude subscription", apiKey: false, terminal: true },
    { id: "console-login", name: "Anthropic Console", description: "Use Anthropic Console (API usage billing)", apiKey: false, terminal: true },
  ],
  codex: [
    { id: "chat-gpt", name: "ChatGPT", description: "Use ChatGPT to authenticate", apiKey: false, terminal: false },
    { id: "api-key", name: "API Key", description: "Use an API key to authenticate", apiKey: true, terminal: false },
  ],
};
/** Fake agents signed in during this page load. The prompt `/logout` signs one out again. */
const fakeSignedIn = new Set<string>();
/** Sessions whose fake agent has its session up. */
const fakeLive = new Set<string>();
const fakeLabel = (agent: string) => FAKE_AGENTS.find((x) => x.id === agent)?.label ?? agent;
/** Agents that take a message mid-turn (the rest queue it behind the turn, as the backend does). */
const FAKE_STEERING = new Set(["patchbay", "grok", "claude-code", "codex"]);
/** Sessions mid-turn, and the prompts queued behind each turn. */
const fakeTurns = new Set<string>();
const fakeQueues: Record<string, { promptId: string; text: string; mentions: string[] }[]> = {};
/** Sessions whose running turn a forced send cancelled. */
const fakeCut = new Set<string>();
/** Sessions whose running turn Stop cancelled; only the `/slow` scenario watches for it. */
const fakeStopped = new Set<string>();

/** Cancel the running turn for a forced send; the front of the queue goes next. */
function fakeInterrupt(csid: string) {
  fakeCut.add(csid);
  for (const [k, r] of pending) { r(null); pending.delete(k); }
}

function fakeEcho(csid: string, promptId: string, text: string, delivery: PromptDelivery) {
  codeEmit(csid, { type: "userPrompt", promptId, text, origin: ME, attachments: [], delivery });
}

/** Run turns back to back: this one, then whatever queued up behind it. */
async function runFakeTurns(csid: string, promptId: string, text: string, mentions: string[]) {
  fakeEcho(csid, promptId, text, "sent");
  fakeTurns.add(csid);
  await fakeAgentTurn(csid, text, mentions);
  if (fakeCut.delete(csid)) codeEmit(csid, { type: "promptDone", stopReason: "cancelled" });
  fakeTurns.delete(csid);
  const next = fakeQueues[csid]?.shift();
  if (next) void runFakeTurns(csid, next.promptId, next.text, next.mentions);
}

/** Agent starts per session, so a sign-in from before a restart is dropped as the real one is. */
const fakeStarts: Record<string, number> = {};

function fakeReady(csid: string, loaded = false) {
  fakeLive.add(csid);
  const agentId = db.code.find((c) => c.id === csid)?.agent ?? "patchbay";
  const grok = usesPatchbayModels(agentId);
  codeEmit(csid, {
    type: "sessionReady", acpSessionId: "acp-" + csid, loaded,
    approvalGaps: grok ? [] : ["This agent edits files in this project without asking, in every mode."],
    capabilities: { loadSession: true, promptCapabilities: { image: agentId === "patchbay", embeddedContext: true }, mcpCapabilities: { http: true, sse: true } },
    agentInfo: { name: "fake-agent" },
    models: grok
      ? { currentModelId: "pb-m1", availableModels: db.models.map((m) => ({ modelId: "pb-" + m.id, name: m.displayName })) }
      : { currentModelId: "sonnet", availableModels: [{ modelId: "sonnet", name: "Sonnet" }, { modelId: "opus", name: "Opus" }] },
    modes: agentId === "patchbay"
      ? { currentModeId: "default", availableModes: [{ id: "default", name: "Default" }, { id: "plan", name: "Plan" }] }
      : grok
      ? null
      : { currentModeId: "build", availableModes: [{ id: "build", name: "Build" }, { id: "plan", name: "Plan" }] },
    configOptions: fakeOptionsFor(csid),
    extensions: { statusLine: agentId === "grok", rewind: grok, patchbayModels: grok, steering: FAKE_STEERING.has(agentId), forceSend: true },
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
}

const cancelled = new Set<string>();

function send<T>(ch: unknown, msg: T) {
  (ch as Channel<T>).onmessage(msg);
}

function codeEmit(codeSessionId: string, event: CodeEnvelope["event"]) {
  emit("code://event", { codeSessionId, event } satisfies CodeEnvelope);
}

/** The ACP wire log (`code/wire.rs`): a few sample frames once turned on. */
const fakeWire: { until: number | null; frames: WireFrame[] } = { until: null, frames: [] };

function fakeWireStatus(): WireLogStatus {
  if (fakeWire.until !== null && Date.now() >= fakeWire.until) fakeWire.until = null;
  const remainingSecs = fakeWire.until === null ? 0 : Math.floor((fakeWire.until - Date.now()) / 1000);
  return { on: fakeWire.until !== null, remainingSecs, frames: fakeWire.frames.length, dropped: 0 };
}

function fakeWireRecord(agent: string, dir: "in" | "out", frame: unknown) {
  if (!fakeWireStatus().on) return;
  const seq = (fakeWire.frames[fakeWire.frames.length - 1]?.seq ?? 0) + 1;
  fakeWire.frames.push({ seq, at: Date.now(), session: "fake", agent, dir, text: JSON.stringify(frame) });
}

/** `?long=<turns>`: a seeded session whose load replays a long transcript with images in it. */
const longTurns = Number(new URLSearchParams(location.search).get("long") ?? 0);
let longSessionId: string | null = null;

/** A noisy PNG, so it stays large the way a screenshot does. */
function noisePng(w: number, h: number, seed: number): string {
  const c = document.createElement("canvas");
  c.width = w;
  c.height = h;
  const ctx = c.getContext("2d")!;
  const img = ctx.createImageData(w, h);
  let x = seed * 2654435761;
  for (let i = 0; i < img.data.length; i++) {
    x = (x * 1103515245 + 12345) >>> 0;
    img.data[i] = i % 4 === 3 ? 255 : x >>> 24;
  }
  ctx.putImageData(img, 0, 0);
  return c.toDataURL("image/png").split(",")[1];
}

/** Each event in its own task, the way the real backend's events arrive. */
const nextTask = () =>
  new Promise<void>((r) => {
    const ch = new MessageChannel();
    ch.port1.onmessage = () => r();
    ch.port2.postMessage(null);
  });

/** Replays `longTurns` turns of reads, commands and screenshots, then reports the session loaded. */
async function fakeLongReplay(csid: string) {
  const out = (n: number) => Array.from({ length: 60 }, (_, j) => `line ${j} of output ${n}: ${"x".repeat(40)}`).join("\n");
  const ev = async (event: CodeEnvelope["event"]) => {
    codeEmit(csid, event);
    await nextTask();
  };
  const t0 = performance.now();
  for (let t = 0; t < longTurns; t++) {
    await ev({ type: "update", update: { sessionUpdate: "user_message_chunk", content: { type: "text", text: `Turn ${t}: keep going.` } } });
    for (const w of "Looking at the code first.".split(/(?<=\s)/)) {
      await ev({ type: "update", update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: w } } });
    }
    for (let k = 0; k < 25; k++) {
      const tid = `long-${t}-${k}`;
      await ev({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: tid, title: `cat src/file${k}.ts`, kind: "execute", status: "in_progress", rawInput: { command: `cat src/file${k}.ts` } } });
      const content = k === 24 && t % 2 === 0
        ? [{ type: "content" as const, content: { type: "image" as const, data: noisePng(480, 320, t * 100 + k), mimeType: "image/png" } }]
        : [{ type: "content" as const, content: { type: "text" as const, text: out(t * 100 + k) } }];
      await ev({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: tid, status: "completed", content } });
    }
    for (const w of `Done with turn ${t}. Here is **what changed** and \`why\`:\n\n- one\n- two\n\n\`\`\`ts\nconst x = ${t};\n\`\`\`\n`.split(/(?<=\s)/)) {
      await ev({ type: "update", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: w } } });
    }
  }
  console.info(`[fake] long replay: ${Math.round(performance.now() - t0)} ms`);
  (window as unknown as { __longReplayMs: number }).__longReplayMs = performance.now() - t0;
}

/** `?harness=1`: a seeded session whose load replays Claude Code's harness messages and a cancel. */
const harnessDemo = new URLSearchParams(location.search).has("harness");
let harnessSessionId: string | null = null;

function fakeHarnessReplay(csid: string) {
  const user = (text: string) => codeEmit(csid, { type: "update", update: { sessionUpdate: "user_message_chunk", content: { type: "text", text } } });
  const agent = (text: string) => codeEmit(csid, { type: "update", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } } });
  user("<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args>opus</command-args>");
  user("<local-command-stdout>Set model to Opus</local-command-stdout>");
  // A prompt and a reminder in one message, the reminder split across chunks.
  user("Fix the flaky upload test.\n<system-reminder>\nThe user opened ");
  user("src/upload.test.ts in the IDE.\n</system-reminder>");
  agent("Looking at the upload test now.");
  user("[Request interrupted by user]");
  user("Use <b>bold</b> in the summary instead.");
  agent("Sure, here is the **summary**.");
  user('<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Background build finished</summary>\n</task-notification>');
  agent("The background build passed.");
  user("[Request interrupted by user for tool use]");
}

/**
 * `?readme=1`: "Windows installer testing" loads the transcript shown in the README's screenshot
 * (`docs/images/code-tab-*.png`). Capture it at 1440×1010 with a device scale of 2.
 */
const readmeDemo = new URLSearchParams(location.search).has("readme");
let readmeSessionId: string | null = null;

function fakeReadmeReplay(csid: string) {
  const s = db.code.find((c) => c.id === csid)!;
  const up = (update: Record<string, unknown>) => codeEmit(csid, { type: "update", update } as CodeEnvelope["event"]);
  const say = (sessionUpdate: string, text: string) => up({ sessionUpdate, content: { type: "text", text } });
  const nsi = `${s.cwd}/packaging/windows/nulamp.nsi`;
  const head = 'Name "nulamp"\nOutFile "${OUTFILE}"\nInstallDir "$PROGRAMFILES64\\nulamp"\n';
  const tail = "RequestExecutionLevel admin\n";
  say("user_message_chunk", "Upgrading with the Windows installer always resets the install folder to the default. Can you fix that and make sure the installer still builds?");
  say("agent_thought_chunk", "The installer probably never reads back the folder it saved. Check the NSIS script.");
  up({ sessionUpdate: "tool_call", toolCallId: "rm-search", title: "Search `InstallDir`", kind: "search", status: "completed", rawInput: { pattern: "InstallDir", path: s.cwd } });
  up({ sessionUpdate: "tool_call", toolCallId: "rm-read", title: "Read `packaging/windows/nulamp.nsi`", kind: "read", status: "completed", rawInput: { path: nsi }, content: [{ type: "content", content: { type: "text", text: head + tail } }] });
  say("agent_message_chunk", "Found it. The installer saves the folder to `HKLM\\Software\\nulamp` but never reads it back, so every upgrade falls back to the default. NSIS has `InstallDirRegKey` for exactly this.\n");
  up({
    sessionUpdate: "tool_call", toolCallId: "rm-edit", title: "Edit `packaging/windows/nulamp.nsi`", kind: "edit", status: "completed",
    rawInput: { file_path: nsi },
    content: [{ type: "diff", path: nsi, oldText: head + tail, newText: head + '; Upgrades reuse the folder the last install chose.\nInstallDirRegKey HKLM "Software\\nulamp" "InstallDir"\n' + tail }],
    locations: [{ path: nsi, line: 1 }],
  });
  up({
    sessionUpdate: "tool_call", toolCallId: "rm-build", title: "Execute `makensis -V2 packaging/windows/nulamp.nsi`", kind: "execute", status: "completed",
    rawInput: { command: "makensis -V2 packaging/windows/nulamp.nsi" },
    content: [{ type: "content", content: { type: "text", text: "Output: \"dist/nulamp-setup.exe\"\nTotal size: 4123456 bytes" } }],
  });
  say("agent_message_chunk", "Fixed. Upgrades now install into whatever folder you picked the first time.\n\n- **Change:** one `InstallDirRegKey` line in `nulamp.nsi`, reading the key the installer already writes.\n- **Checked:** `makensis` builds the setup with no warnings.\n\nFresh installs still default to `Program Files\\nulamp`.");
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
  const reply = text.includes("wide table") ? WIDE_TABLE : [
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
  // A forced send cut this turn short: it runs out silently, as if the agent stopped.
  const turnEmit = (e: CodeEnvelope["event"]) => {
    if (!fakeCut.has(csid)) codeEmit(csid, e);
  };
  const say = async (t: string) => {
    for (const w of t.split(/(?<=\s)/)) {
      await sleep(15);
      turnEmit({ type: "update", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: w } } });
    }
  };
  fakeWireRecord(s.agent, "out", { jsonrpc: "2.0", id: 3, method: "session/prompt", params: { sessionId: "acp", prompt: [{ type: "text", text }] } });
  turnEmit({ type: "update", update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Planning the change." } } });
  // `/slow`: a long turn with quiet stretches, for the Working… readout. Thoughts stream, then 8s
  // of silence (a counter appears), a 6s tool (never "stalled"), some text, then 40s of silence
  // (the stall warning and its Stop button). Stop ends it.
  if (text.startsWith("/slow")) {
    fakeStopped.delete(csid);
    const nap = async (ms: number) => {
      for (let t0 = 0; t0 < ms && !fakeStopped.has(csid); t0 += 200) await sleep(200);
      return fakeStopped.has(csid);
    };
    const stop = () => {
      fakeStopped.delete(csid);
      turnEmit({ type: "promptDone", stopReason: "cancelled" });
    };
    for (const w of "Working out how to split this into steps, then which files each step touches. ".split(/(?<=\s)/)) {
      turnEmit({ type: "update", update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: w } } });
      await sleep(250);
    }
    if (await nap(8000)) return stop();
    const toolId = id();
    turnEmit({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: toolId, title: "Execute `cargo build`", kind: "execute", status: "in_progress", rawInput: { command: "cargo build" } } });
    if (await nap(6000)) return stop();
    turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: toolId, status: "completed", content: [{ type: "content", content: { type: "text", text: "Finished `dev` profile" } }] } });
    await say("Build is clean. Now checking the remaining modules, one at a time. ");
    if (await nap(40_000)) return stop();
    await say("Done after a long wait.");
    turnEmit({ type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // Frames ACP can't decode (see `code/wire.rs`): an unknown update type, a tool status outside
  // the four known ones, and a permission option of an unknown kind.
  if (text.startsWith("/odd")) {
    const raw = { sessionUpdate: "turn_summary", summary: { files: 2, note: "Edited README.md" } };
    fakeWireRecord(s.agent, "in", { jsonrpc: "2.0", method: "session/update", params: { sessionId: "acp", update: raw } });
    for (let n = 0; n < 2; n++) {
      codeEmit(csid, { type: "unrecognized", message: `The agent sent an update Patchbay doesn't know ("turn_summary")`, raw });
    }
    const toolId = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: toolId, title: "Run `npm test`", kind: "execute", status: "cancelled", rawInput: { command: "npm test" } } });
    const reqId = id();
    const toolId2 = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: toolId2, title: "Edit `README.md`", kind: "edit", status: "pending", content: [{ type: "diff", path: `${s.cwd}/README.md`, oldText: "Old intro." }] } });
    const choice = await new Promise<string | null>((resolve) => {
      pending.set(reqId, resolve);
      codeEmit(csid, {
        type: "permission",
        requestId: reqId,
        request: {
          sessionId: "acp",
          toolCall: { toolCallId: toolId2, title: "Edit `README.md`", kind: "edit" },
          options: [
            { optionId: "once", name: "Allow", kind: "allow_once" },
            { optionId: "session", name: "Allow for this session", kind: "allow_for_session" as "allow_once" },
            { optionId: "no", name: "Reject", kind: "reject_once" },
          ],
        },
      });
    });
    codeEmit(csid, { type: "permissionResolved", requestId: reqId });
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: toolId2, status: choice === "no" ? "failed" : "completed" } });
    await say(`Picked: ${choice}`);
    codeEmit(csid, { type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // A plan that waits for approval, the way Claude Code's `ExitPlanMode` asks.
  if (text.startsWith("/plan-approve")) {
    const plan = `# Tidy the README\n\n## Steps\n\n1. Read \`README.md\`.\n2. Rewrite the intro.\n`;
    const planId = id();
    const reqId = id();
    turnEmit({
      type: "update",
      update: { sessionUpdate: "tool_call", toolCallId: planId, title: "Ready to code?", kind: "switch_mode", rawInput: { plan }, content: [{ type: "content", content: { type: "text", text: plan } }] },
    });
    const choice = await new Promise<string | null>((resolve) => {
      pending.set(reqId, resolve);
      turnEmit({
        type: "permission",
        requestId: reqId,
        request: {
          sessionId: "acp",
          toolCall: { toolCallId: planId, title: "Ready to code?", kind: "switch_mode" },
          options: [
            { optionId: "default", name: "Yes, and manually approve edits", kind: "allow_once" },
            { optionId: "plan", name: "No, keep planning", kind: "reject_once" },
          ],
        },
      });
    });
    codeEmit(csid, { type: "permissionResolved", requestId: reqId });
    turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: planId, status: choice === "default" ? "completed" : "failed" } });
    await say(choice === "default" ? "Starting on it." : "Okay, still planning.");
    turnEmit({ type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // An edit sent the way Claude Code sends one: a diff per hunk, each hunk's new start line in `locations`.
  if (text.startsWith("/hunks")) {
    const path = `${s.cwd}/src/main.ts`;
    codeEmit(csid, {
      type: "update",
      update: {
        sessionUpdate: "tool_call",
        toolCallId: id(),
        title: "Edit `src/main.ts`",
        kind: "edit",
        status: "completed",
        rawInput: { file_path: path },
        content: [
          { type: "diff", path, oldText: "import { a } from \"./a\";\nimport { b } from \"./b\";\n\nconst x = 1;", newText: "import { a } from \"./a\";\nimport { b } from \"./b\";\nimport { c } from \"./c\";\n\nconst x = 1;" },
          { type: "diff", path, oldText: "function run() {\n  a();\n  b();\n  return x;\n}", newText: "function run() {\n  a();\n  b();\n  c();\n  return x;\n}" },
        ],
        locations: [{ path, line: 10 }, { path, line: 40 }],
      },
    });
    await say("Done.");
    codeEmit(csid, { type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // A plan document written as a tool call, the way Cursor's plan mode does it.
  if (text.startsWith("/plan")) {
    const plan = `# Tidy the README\n\n## Problem\n\nThe intro is out of date.\n\n## Steps\n\n1. Read \`README.md\`.\n2. Rewrite the intro.\n3. Check the links still work.\n`;
    turnEmit({
      type: "update",
      update: {
        sessionUpdate: "tool_call",
        toolCallId: id(),
        title: "Create Plan: Tidy the README",
        status: "completed",
        rawInput: { _toolName: "createPlan", name: "Tidy the README", plan },
        content: [{ type: "content", content: { type: "text", text: "Plan saved to file:///tmp/tidy-the-readme.plan.md" } }],
      },
    });
    await say("The plan is ready. Say the word and I'll start.");
    turnEmit({ type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // Claude Code's diffs: an Edit's hunks (start lines in `locations`), a Write that first sends
  // the whole file with `oldText: null`, and a Codex-style deletion (`newText: ""`).
  if (text.startsWith("/hunks")) {
    const app = `${s.cwd}/src/app.ts`;
    const editId = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: editId, title: "Edit src/app.ts", kind: "edit", status: "pending", rawInput: { file_path: app }, content: [{ type: "diff", path: app, oldText: "return 1;", newText: "return one;" }], locations: [{ path: app }] } });
    await sleep(200);
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, status: "completed",
      content: [
        { type: "diff", path: app, oldText: "import { b } from \"./b\";\n\nfunction one() {\n  return 1;\n}", newText: "import { b } from \"./b\";\n\n/** The first. */\nfunction one() {\n  return one;\n}" },
        { type: "diff", path: app, oldText: "function two() {\n  return 2;\n}\n\nexport { one, two };", newText: "function two() {\n  return b(2);\n}\n\nexport { one, two };" },
      ],
      locations: [{ path: app, line: 2 }, { path: app, line: 9 }] } });
    // A second edit to the same file, by a relative path, over the first hunk.
    const edit2 = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: edit2, title: "Edit src/app.ts", kind: "edit", status: "completed",
      content: [{ type: "diff", path: "./src/app.ts", oldText: "/** The first. */\nfunction one() {\n  return one;", newText: "/** The first one. */\nfunction one() {\n  return one;" }],
      locations: [{ path: "./src/app.ts", line: 4 }] } });
    const notes = `${s.cwd}/NOTES.md`;
    const writeId = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: writeId, title: "Write NOTES.md", kind: "edit", status: "pending", rawInput: { file_path: notes }, content: [{ type: "diff", path: notes, oldText: null, newText: "# Notes\n\n- one\n- two, revised\n- three\n" }], locations: [{ path: notes }] } });
    await sleep(200);
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: writeId, status: "completed", content: [{ type: "diff", path: notes, oldText: "# Notes\n\n- one\n- two\n- three", newText: "# Notes\n\n- one\n- two, revised\n- three" }], locations: [{ path: notes, line: 1 }] } });
    const newId = id();
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: newId, title: "Write src/b.ts", kind: "edit", status: "completed", content: [{ type: "diff", path: `${s.cwd}/src/b.ts`, oldText: null, newText: "export const b = (n: number) => n;\n" }], locations: [{ path: `${s.cwd}/src/b.ts` }] } });
    codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: id(), title: "Delete old.txt", kind: "delete", status: "completed", content: [{ type: "diff", path: `${s.cwd}/old.txt`, oldText: "stale\n", newText: "" }] } });
    await say("Edited `src/app.ts` in two places, rewrote `NOTES.md`, added `src/b.ts` and removed `old.txt`.");
    codeEmit(csid, { type: "promptDone", stopReason: "end_turn" });
    return;
  }
  // OpenCode's (and Kilo's) to-do list: `todowrite` tool calls, not ACP plans. The first write
  // completes, the second is rejected, the third saves a list with a cancelled step.
  if (text.startsWith("/todos")) {
    const write = async (todos: { content: string; status: string; priority: string }[], ok: boolean) => {
      const tid = id();
      codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call", toolCallId: tid, title: "todowrite", kind: "other", status: "pending", rawInput: {} } });
      await sleep(150);
      codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: tid, status: "in_progress", rawInput: { todos } } });
      await sleep(150);
      if (!ok) {
        codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: tid, status: "failed", rawOutput: { error: "The user rejected permission to use this specific tool call." } } });
        return;
      }
      const open = todos.filter((t) => t.status !== "completed").length;
      codeEmit(csid, { type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: tid, status: "completed", title: `${open} todos`, rawOutput: { output: JSON.stringify(todos, null, 2), metadata: { todos } } } });
    };
    await write([
      { content: "Read the parser", status: "in_progress", priority: "high" },
      { content: "Fix the off-by-one", status: "pending", priority: "high" },
      { content: "Rewrite the lexer", status: "pending", priority: "low" },
    ], true);
    await say("Reading the parser first.\n");
    await write([{ content: "Delete every test", status: "pending", priority: "high" }], false);
    await say("That write was rejected, so the plan stays.\n");
    await write([
      { content: "Read the parser", status: "completed", priority: "high" },
      { content: "Fix the off-by-one", status: "in_progress", priority: "high" },
      { content: "Rewrite the lexer", status: "cancelled", priority: "low" },
    ], true);
    await say("Dropped the lexer rewrite; fixing the off-by-one now.");
    codeEmit(csid, { type: "promptDone", stopReason: "end_turn" });
    return;
  }
  turnEmit({
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
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: readId, title: "Read `README.md`", kind: "read", status: "completed", rawInput: { path: `${s.cwd}/README.md` }, content: [{ type: "content", content: { type: "text", text: "# demo\n\nOld intro." } }] } });
  const editId = id();
  const diff = { type: "diff" as const, path: `${s.cwd}/README.md`, oldText: "# demo\n\nOld intro.\n", newText: "# demo\n\nNew intro written by the agent.\n\n## Usage\n\nRun it.\n" };
  // Like the real backend: the announcement leaves out the defaults (`pending`, `other`), and the
  // diff and full rawInput arrive only with the permission request.
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: editId, title: "Edit" } });
  const editCall = { toolCallId: editId, title: "Edit `README.md`", kind: "edit", rawInput: { file_path: "README.md" }, content: [diff] };
  // The backend passes a known call's permission-request fields on as an update.
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", ...editCall } });
  const reqId = id();
  const rule = db.rules.find((r) => r.toolKind === "edit" && (r.cwd === "*" || r.cwd === s.cwd));
  const choice = rule
    ? (turnEmit({ type: "autoApproved", toolCallId: editId, description: "Auto-approved: all edit calls in this folder" }), "allow-once")
    : await new Promise<string | null>((resolve) => {
    pending.set(reqId, resolve);
    turnEmit({
      type: "permission",
      requestId: reqId,
      request: {
        sessionId: "acp",
        toolCall: { ...editCall, status: "pending" },
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
    turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, status: "failed" } });
    await say("Okay, I won't make that edit.");
    turnEmit({ type: "promptDone", stopReason: choice ? "end_turn" : "cancelled" });
    return;
  }
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: editId, status: "completed" } });
  const runId = id();
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: runId, title: "Execute `ls`", kind: "execute", status: "in_progress", rawInput: { command: "ls" } } });
  await sleep(300);
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call_update", toolCallId: runId, status: "completed", content: [{ type: "content", content: { type: "text", text: "README.md\nsrc\nCargo.toml" } }] } });
  const termId = id();
  turnEmit({ type: "update", update: { sessionUpdate: "tool_call", toolCallId: termId, title: "Execute `cargo check`", kind: "execute", status: "completed", rawInput: { command: "cargo check" }, content: [{ type: "terminal", terminalId: "term-1" }], rawOutput: { stdout: "Finished `dev` profile in 0.6s", exit_code: 0 } } });
  turnEmit({ type: "update", update: { sessionUpdate: "plan", entries: [
    { content: "Read README.md", priority: "high", status: "completed" },
    { content: "Edit README.md", priority: "high", status: "completed" },
    { content: "Run ls", priority: "medium", status: "completed" },
  ] } });
  await say("Done. I updated the README intro and added a **Usage** section.");
  fakeUsage.input += 9000; fakeUsage.cachedRead += 6000; fakeUsage.output += 700;
  turnEmit({ type: "usage", usage: { totals: { ...fakeUsage }, contextTokens: Math.min(118000, fakeUsage.input / 2), contextWindow: 128000, autoCompactPercent: 85 } });
  turnEmit({ type: "promptDone", stopReason: "end_turn" });
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
      { nodeId: ME, name: mesh.name, state: "self", via: null, viaName: null, rttMs: null, url: null, admittedAt: now() - 9e8, approver: ME, appVersion: "0.7.2" },
      { nodeId: "n-studio", name: "studio", state: "direct", via: null, viaName: null, rttMs: 4, url: "ws://100.64.0.7:41890", admittedAt: now() - 8e8, approver: ME, appVersion: "0.7.2" },
      { nodeId: "n-nas", name: "nas", state: "relay", via: "n-studio", viaName: "studio", rttMs: null, url: null, admittedAt: now() - 7e8, approver: "n-studio", appVersion: "0.7.1" },
      {
        nodeId: "n-phone",
        name: "pocket",
        state: "relay",
        via: "n-studio",
        viaName: "studio",
        rttMs: null,
        url: null,
        admittedAt: now() - 6.5e8,
        approver: "n-studio",
        role: "remote",
      },
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

// ---- the phone app (remote.html): a phone connected through "studio"; `?join=1` starts unjoined ----

const PHONE = "n-phone";
const phoneApp = (window as { __PATCHBAY_REMOTE__?: boolean }).__PATCHBAY_REMOTE__ === true;
const phone = {
  nodeId: PHONE,
  name: "pixel",
  member: !new URLSearchParams(location.search).has("join"),
  meshId: "demo-mesh" as string | null,
  phase: "online",
  error: null as string | null,
  gateway: "n-studio",
  gatewayName: "studio",
  gatewayUrl: "ws://192.168.1.20:41890",
  rttMs: 12,
  resumed: true,
  joining: null as { url: string; code: string | null; status: string; error: string | null } | null,
  members: [] as MeshStatus["members"],
  pending: [] as MeshStatus["pending"],
};
// A copy each time, as the real backend's JSON would be.
const phoneChanged = () => emit("remote://status", structuredClone(phone));

function phoneMesh() {
  meshMember();
  mesh.nodeId = PHONE;
  mesh.name = phone.name;
  mesh.members = [
    { ...mesh.members[0], nodeId: PHONE, name: phone.name, state: "self", role: "remote" },
    { ...mesh.members[0], nodeId: ME, name: "laptop", state: "relay" },
    ...mesh.members.slice(1).map((m) => (m.nodeId === "n-studio" ? { ...m, state: "direct" as const } : m)),
  ];
  phone.members = mesh.members;
}

const CLAUDE_SESSIONS = [
  { id: "c1", cwd: "/home/demo/projects/demo-app", cwdExists: true, title: "Fix login redirect", preview: "The login redirect loops when the session cookie expires. Can you fix it?", modified: Date.now() - 3_600_000, size: 412_000, imported: null },
  { id: "c2", cwd: "/home/demo/projects/demo-app", cwdExists: true, title: "Add dark mode", preview: "Add a dark mode toggle to settings", modified: Date.now() - 86_400_000 * 3, size: 1_800_000, imported: null },
  { id: "c3", cwd: "/home/demo/projects/old-worktree", cwdExists: false, title: "Refactor parser", preview: "Split the parser into a lexer and a parser", modified: Date.now() - 86_400_000 * 20, size: 90_000, imported: null },
];

export function install() {
  mockWindows("main");
  if (phoneApp) {
    phoneMesh();
    // `?joinRequest=1`: a computer asks to join, waiting on studio. The phone may decide it.
    if (new URLSearchParams(location.search).has("joinRequest")) {
      phone.pending = [{ nodeId: "n-new", name: "travel-pc", code: "482 913", host: "n-studio", hostName: "studio" }];
    }
  }
  // Seed one provider + model so the UI starts usable.
  const p: Provider = { id: "p1", name: "Demo provider", kind: "openai_chat", baseUrl: "http://localhost:8765/v1", hasKey: true, authMode: "api_key" };
  db.providers.push(p);
  db.models.push({ id: "m1", providerId: "p1", modelId: "demo-model", displayName: "Demo model", vision: true, contextWindow: 128000, params: { ...emptyParams(), pricing: { input: 3, cachedInput: 0.3, cacheWrite: 3.75, output: 15 } } });
  db.models.push({ id: "m2", providerId: "p1", modelId: "demo-mini", displayName: "Demo mini", vision: false, contextWindow: 32000, params: emptyParams() });
  const seed = (cwd: string, title: string, ago: number, activity: CodeSession["activity"], hostNode = ME) =>
    db.code.push({ id: id(), cwd, acpSessionId: null, title, modelRef: "m1", pinned: false, createdAt: now() - ago, updatedAt: now() - ago, activity, hostNode, epoch: 0, agent: "patchbay", agentModel: null, permissionMode: "default" });
  seed("/home/demo/Projects/nulamp", "Windows installer testing", 1000, "edit");
  if (readmeDemo) {
    readmeSessionId = db.code[db.code.length - 1].id;
    Object.assign(db.models[0], { modelId: "qwen3-coder", displayName: "Qwen3 Coder" });
    // A registry key, not a file: keep it from rendering as a file link.
    (window as unknown as Record<string, unknown>).__fakeMissing = ["HKLM\\Software\\nulamp"];
  }
  seed("/home/demo/Projects/nulamp", "Release to staging", 5000, "pr");
  // A second folder on the phone's laptop, so project drag has two rows to swap.
  if (phoneApp) seed("/home/demo/Projects/patchbay", "Gatekeeper quarantine", 2000, "edit");
  seed("/home/demo/Projects/omnimark", "Interactive document controls", 3000, "branch", meshDemo ? "n-studio" : ME);
  seed("/home/demo/Work/omnimark", "Session overview", 9000, null, meshDemo ? "n-old" : ME);
  if (longTurns > 0) {
    seed("/home/demo/Projects/ips-amp-admin", "Long transcript", 10, "edit");
    longSessionId = db.code[db.code.length - 1].id;
  }
  if (harnessDemo) {
    seed("/home/demo/Projects/uploads", "Claude Code replay", 5, "edit");
    harnessSessionId = db.code[db.code.length - 1].id;
  }
  if (meshDemo) {
    // The phone already built its mesh in phoneMesh(). Rebuilding it here would
    // name the laptop with the phone's name, so its sessions show up under the phone.
    if (!phoneApp) meshMember();
    mesh.pending = [{ nodeId: "n-new", name: "travel-pc", code: "482 913" }];
    setTimeout(fakeRemoteStream, 2500);
  }
  // `?mesh=1&meshError=1`: studio rejects this computer's passphrase, so nothing else is reachable.
  if (meshDemo && new URLSearchParams(location.search).has("meshError")) {
    mesh.pending = [];
    for (const m of mesh.members) {
      if (m.state === "self") continue;
      Object.assign(m, { state: "offline", via: null, viaName: null, rttMs: null, authFailed: m.nodeId === "n-studio" });
    }
    mesh.problem = {
      kind: "auth",
      title: "studio rejected this computer's passphrase",
      message: "This computer can't sync with it until the passphrases match. Re-enter the passphrase in Settings → Mesh.",
    };
    setTimeout(() => {
      emit("mesh://error", mesh.problem);
      meshStatusChanged();
    }, 800);
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
          const i = a.input as {
            id?: string;
            name: string;
            kind: Provider["kind"];
            baseUrl: string;
            apiKey?: string;
            authMode?: Provider["authMode"];
          };
          let x = db.providers.find((q) => q.id === i.id);
          if (!x) {
            x = { id: id(), name: "", kind: i.kind, baseUrl: "", hasKey: false, authMode: "api_key" };
            db.providers.push(x);
          }
          const authMode = i.kind === "anthropic" && i.authMode === "claude_login" ? "claude_login" : "api_key";
          Object.assign(x, { name: i.name, kind: i.kind, baseUrl: i.baseUrl, authMode });
          if (authMode === "claude_login") x.hasKey = false;
          else if (i.apiKey !== undefined) x.hasKey = i.apiKey !== "";
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
          const p = a.prefs as {
            terminalShell?: string;
            autoCompactPercent?: number | null;
            usageLimit?: { percent: number };
            readAloudEngine?: string;
            readAloudRate?: string;
            readAloudServerUrl?: string;
          };
          if (p.terminalShell && !p.terminalShell.startsWith("/")) throw `terminal shell ${p.terminalShell} does not exist`;
          if (p.autoCompactPercent != null && (p.autoCompactPercent < 10 || p.autoCompactPercent > 99)) throw "auto-compact threshold must be between 10 and 99%";
          if (p.usageLimit && (p.usageLimit.percent < 1 || p.usageLimit.percent > 100)) throw "usage limit must be between 1 and 100%";
          if (p.readAloudEngine && !["auto", "system", "builtin", "server"].includes(p.readAloudEngine)) throw "read aloud engine must be one of auto, system, builtin, server";
          if (p.readAloudRate && !["slower", "normal", "faster"].includes(p.readAloudRate)) throw "read aloud speed must be one of slower, normal, faster";
          const speechUrl = p.readAloudServerUrl?.trim() ?? "";
          if (speechUrl && !speechUrl.startsWith("http://") && !speechUrl.startsWith("https://")) throw "speech server URL must be an http or https URL";
          db.settings.preferences = JSON.stringify(a.prefs);
          return null;
        }
        case "list_native_voices": return [];
        case "speak_native": return null;
        case "stop_native": return null;
        case "rewrite_for_speech": throw "pnpm dev has no model to rewrite speech. Run the app.";
        case "synthesize_speech": throw "pnpm dev has no speech server. Use System or Built-in, or run the app.";
        case "claude_overview": {
          const cfg = JSON.parse(db.settings.preferences ?? "{}").claudeConfig ?? {};
          const off = (list: string[] | undefined, x: string) => (list ?? []).includes(x);
          return {
            home: "/home/me/.claude",
            project: (a.cwd as string | null) ?? null,
            instructions: ["/home/me/.claude/CLAUDE.md", `${a.cwd ?? "/home/me/src/patchbay"}/CLAUDE.md`],
            plugins: [
              { id: "caveman@caveman", name: "caveman", enabledInClaude: true, disabled: off(cfg.disabledPlugins, "caveman@caveman") },
              { id: "lsp@tools", name: "lsp", enabledInClaude: false, disabled: off(cfg.disabledPlugins, "lsp@tools") },
            ],
            commands: [
              { name: "review", description: "Review the current changes", source: "project" },
              { name: "caveman:compress", description: "Compress a file caveman-style", source: "plugin caveman" },
            ],
            skills: [{ name: "pdf", description: "Work with PDF files", source: "user" }],
            agents: [{ name: "cavecrew-builder", description: "Surgical 1-2 file edit", source: "plugin caveman" }],
            mcpServers: [
              { name: "github", source: "user", transport: "http", disabled: off(cfg.disabledMcpServers, "github") },
              { name: "postgres", source: "project", transport: "stdio", disabled: off(cfg.disabledMcpServers, "postgres") },
            ],
            hooks: [
              { event: "SessionStart", matcher: "", command: "node \"$CLAUDE_PLUGIN_ROOT/src/hooks/caveman-activate.js\"", source: "plugin caveman" },
              { event: "PreToolUse", matcher: "Edit|Write", command: "./scripts/lint-staged.sh", source: "project" },
            ],
          };
        }
        case "web_search_status": return { configured: !!db.settings["web_search.key"], source: db.settings["web_search.key"] ? "keychain" : null };
        case "set_web_search_key": db.settings["web_search.key"] = a.key as string; return null;
        case "clear_web_search_key": delete db.settings["web_search.key"]; return null;
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
          if (a.archived != null) c.archivedAt = a.archived ? now() : null;
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
        case "attachment_thumbnail":
        case "attachment_image": {
          const n = cmd === "attachment_image" ? 512 : 64;
          const c = document.createElement("canvas");
          c.width = c.height = n;
          const g = c.getContext("2d")!;
          g.fillStyle = "#e5534b"; g.fillRect(0, 0, n, n);
          g.fillStyle = "#fff"; g.fillRect(n / 4, n / 4, n / 2, n / 2);
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
        // Paths under /home/demo, or relative to a session folder, exist; set window.__fakeMissing
        // to a list of paths that don't.
        case "local_path_exists": {
          const p = String(a.path);
          const missing = ((window as unknown as Record<string, unknown>).__fakeMissing as string[] | undefined) ?? [];
          return !missing.includes(p) && (p.startsWith("/home/demo") || p.startsWith("~/") || (!p.startsWith("/") && !!a.cwd));
        }
        case "open_local_path":
          console.info("fake backend: open path", a.path, "in", a.cwd);
          (window as unknown as Record<string, unknown>).__lastOpenedPath = { path: a.path, cwd: a.cwd };
          return null;
        // Set window.__fakeUpdate = { version, notes } before the first check to see the update button.
        case "update_check":
          return (window as unknown as Record<string, unknown>).__fakeUpdate ?? null;
        case "update_install":
          console.info("fake backend: install update");
          await new Promise((r) => setTimeout(r, 1500));
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
        case "list_claude_sessions":
          return CLAUDE_SESSIONS.map((c) => ({ ...c, imported: db.settings[`import.claude.${c.id}`] ?? null }));
        case "import_claude_sessions": {
          const made: CodeSession[] = [];
          for (const cid of a.ids as string[]) {
            const c = CLAUDE_SESSIONS.find((x) => x.id === cid);
            if (!c) continue;
            const prior = db.code.find((x) => x.id === db.settings[`import.claude.${cid}`]);
            if (prior) {
              made.push(prior);
              continue;
            }
            const s: CodeSession = {
              id: id(), cwd: c.cwd, acpSessionId: null, title: c.title, modelRef: null, pinned: false,
              createdAt: c.modified - 3_600_000, updatedAt: c.modified, activity: null, agent: (a.agent as AgentId | null) ?? "patchbay",
              agentModel: null, hostNode: ME, epoch: 0, permissionMode: "default", titleManual: true,
            };
            db.code.push(s);
            db.settings[`import.claude.${cid}`] = s.id;
            db.transcripts[s.id] = JSON.stringify({
              v: 1,
              earlier: {
                entries: [
                  { kind: "user", text: c.preview },
                  { kind: "agent", text: "Looking at it now." },
                  { kind: "tool", id: "t1" },
                  { kind: "agent", text: "Fixed." },
                  { kind: "info", text: "Imported from Claude Code. The agent gets this conversation as context with your first message." },
                ],
                toolCalls: { t1: { toolCallId: "t1", title: "Edit src/app.ts", kind: "edit", status: "completed" } },
              },
              entries: [],
              toolCalls: {},
            });
            made.push(s);
          }
          return made;
        }
        case "create_code_session": {
          const prefs = JSON.parse(db.settings.preferences ?? "{}") as { defaultAgent?: AgentId; permissionMode?: CodeSession["permissionMode"] };
          const s: CodeSession = {
            id: id(),
            cwd: a.cwd as string,
            acpSessionId: null,
            title: "New session",
            modelRef: (a.modelRef as string) ?? null,
            pinned: false,
            createdAt: now(),
            updatedAt: now(),
            activity: null,
            agent: ((a.agent as AgentId | null) ?? prefs.defaultAgent ?? "patchbay"),
            agentModel: null,
            hostNode: (a.host as string | null) ?? ME,
            epoch: 0,
            permissionMode: prefs.permissionMode ?? "default",
          };
          db.code.push(s);
          return s;
        }
        case "code_set_permission_mode": {
          const s = db.code.find((x) => x.id === a.id)!;
          s.permissionMode = a.mode as CodeSession["permissionMode"];
          s.updatedAt = now();
          return null;
        }
        case "update_code_session": {
          const s = db.code.find((x) => x.id === a.id)!;
          if (a.title != null) {
            s.title = a.title as string;
            s.titleManual = true;
          }
          if (a.pinned != null) s.pinned = a.pinned as boolean;
          if (a.activity != null) s.activity = a.activity as CodeSession["activity"];
          if (a.archived != null) s.archivedAt = a.archived ? now() : null;
          return null;
        }
        case "delete_code_session": db.code = db.code.filter((x) => x.id !== a.id); return null;
        case "code_is_running": return false;
        case "code_start": {
          const csid = a.id as string;
          fakeRunning.add(csid);
          fakeLive.delete(csid);
          fakeStarts[csid] = (fakeStarts[csid] ?? 0) + 1;
          setTimeout(() => {
            codeEmit(csid, { type: "status", state: "starting", message: null });
            setTimeout(() => {
              const agent = db.code.find((c) => c.id === csid)?.agent ?? "patchbay";
              const methods = FAKE_AUTH[agent];
              if (methods && !fakeSignedIn.has(agent)) {
                codeEmit(csid, { type: "authRequired", message: `${fakeLabel(agent)} needs you to sign in first.`, methods });
                return;
              }
              if (csid === longSessionId) void fakeLongReplay(csid).then(() => fakeReady(csid, true));
              else if (csid === harnessSessionId) {
                fakeHarnessReplay(csid);
                fakeReady(csid, true);
              } else if (csid === readmeSessionId) {
                fakeReadmeReplay(csid);
                fakeReady(csid, true);
                codeEmit(csid, { type: "usage", usage: { totals: { input: 41000, cachedRead: 30000, cacheWrite: 0, output: 1800, reasoning: 0, costUsd: 0.07, apiMs: 0 }, contextTokens: 24000, contextWindow: 128000, autoCompactPercent: 85 } });
              } else fakeReady(csid);
            }, 400);
          }, 10);
          return null;
        }
        case "code_authenticate": {
          const csid = a.id as string;
          const agent = db.code.find((c) => c.id === csid)?.agent ?? "patchbay";
          const m = FAKE_AUTH[agent]?.find((x) => x.id === a.methodId && !x.terminal);
          if (!m) throw "agent is not running for this session";
          // A browser sign-in takes a while; a key "bad" is refused.
          const start = fakeStarts[csid];
          await sleep(m.apiKey ? 400 : 2500);
          if (fakeStarts[csid] !== start) throw "the agent stopped";
          if (m.apiKey && a.apiKey === "bad") throw "Sign-in failed: Authentication required (API key not valid)";
          fakeSignedIn.add(agent);
          if (fakeLive.has(csid)) {
            // Mid-session: the session goes on.
            codeEmit(csid, { type: "authResolved" });
            codeEmit(csid, { type: "notice", message: "Signed in. Send your message again." });
          } else fakeReady(csid);
          return null;
        }
        case "code_stop": return null;
        case "code_prompt": {
          // Like the backend: an unnamed session takes its first prompt's first line.
          const s = db.code.find((x) => x.id === a.id)!;
          const line = (a.text as string).split("\n").find((l) => l.trim())?.trim();
          if (line && !s.titleManual && s.title === "New session") {
            s.title = line.length > 60 ? `${line.slice(0, 60)}…` : line;
            codeEmit(s.id, { type: "title", title: s.title });
          }
          // Signed out mid-session (`/logout` fakes it): the prompt is refused for a sign-in.
          const methods = FAKE_AUTH[s.agent];
          if (methods && (a.text === "/logout" || !fakeSignedIn.has(s.agent))) {
            fakeSignedIn.delete(s.agent);
            setTimeout(() => {
              codeEmit(s.id, { type: "authRequired", message: `${fakeLabel(s.agent)} needs you to sign in again.`, methods });
              codeEmit(s.id, { type: "promptDone", stopReason: "error" });
            }, 300);
            return null;
          }
          const promptId = (a.promptId as string) || id();
          const mentions = (a.mentions as string[]) ?? [];
          if (fakeTurns.has(s.id) && a.force) {
            (fakeQueues[s.id] ??= []).unshift({ promptId, text: a.text as string, mentions });
            fakeEcho(s.id, promptId, a.text as string, "queued");
            fakeInterrupt(s.id);
            return null;
          }
          if (fakeTurns.has(s.id)) {
            if (FAKE_STEERING.has(s.agent)) {
              fakeEcho(s.id, promptId, a.text as string, "steered");
              codeEmit(s.id, { type: "update", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `\n\n(Noted mid-turn: ${a.text})\n\n` } } });
            } else {
              (fakeQueues[s.id] ??= []).push({ promptId, text: a.text as string, mentions });
              fakeEcho(s.id, promptId, a.text as string, "queued");
            }
            return null;
          }
          void runFakeTurns(s.id, promptId, a.text as string, mentions);
          return null;
        }
        case "code_unqueue": {
          const q = fakeQueues[a.id as string] ?? [];
          const i = q.findIndex((x) => x.promptId === a.promptId);
          if (i >= 0) {
            q.splice(i, 1);
            codeEmit(a.id as string, { type: "unqueued", promptIds: [a.promptId as string], restore: false });
          }
          return null;
        }
        case "code_send_now": {
          const q = fakeQueues[a.id as string] ?? [];
          const i = q.findIndex((x) => x.promptId === a.promptId);
          if (i >= 0 && fakeTurns.has(a.id as string)) {
            q.unshift(...q.splice(i, 1));
            fakeInterrupt(a.id as string);
          }
          return null;
        }
        case "code_auto_title": {
          const s = db.code.find((x) => x.id === a.id)!;
          await sleep(400);
          s.title = "Fake generated title";
          s.titleManual = false;
          codeEmit(s.id, { type: "title", title: s.title });
          return s.title;
        }
        case "get_code_transcript": return db.transcripts[a.id as string] ?? null;
        case "save_code_transcript": db.transcripts[a.id as string] = a.json as string; return null;
        case "list_project_files": await sleep(80); return DEMO_FILES;
        case "code_session_image": return "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
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
            code: [
              ...db.code.slice(0, 2).map((s) => ({ id: s.id, title: s.title, cwd: s.cwd, modelRef: "m1", agent: "patchbay", agentModel: null, updatedAt: s.updatedAt, usage: { ...fakeUsage, input: 90000, output: 7000 } })),
              // An agent that brings its own model: labeled by agent, priced only by what it reports.
              { id: "fake-cc-usage", title: "Claude Code session", cwd: "/home/me/src/patchbay", modelRef: "m1", agent: "claude-code", agentModel: "opus", updatedAt: Date.now(), usage: { ...fakeUsage, input: 120000, output: 9000, costUsd: 1.23 } },
            ],
          };
        }
        case "quick_submit": case "quick_dismiss": return null;
        // No native webview in the browser: approximate page zoom with CSS.
        case "plugin:webview|set_webview_zoom": document.documentElement.style.zoom = String(a.value); return null;
        case "plugin:notification|is_permission_granted": return true;
        case "plugin:notification|notify": console.info("fake backend: notification", a); return null;
        case "plugin:clipboard-manager|write_text": return navigator.clipboard.writeText(a.text as string).then(() => null);
        case "plugin:clipboard-manager|read_text": return navigator.clipboard.readText();
        case "code_cancel": {
          fakeCut.delete(a.id as string);
          if (fakeTurns.has(a.id as string)) fakeStopped.add(a.id as string);
          for (const [k, r] of pending) { r(null); pending.delete(k); }
          const dropped = (fakeQueues[a.id as string] ?? []).splice(0);
          if (dropped.length) codeEmit(a.id as string, { type: "unqueued", promptIds: dropped.map((d) => d.promptId), restore: true });
          return null;
        }
        case "code_set_model": case "code_set_mode": return null;
        case "code_set_config_option": {
          const csid = a.id as string;
          fakeConfig[csid] = { ...fakeConfig[csid], [a.configId as string]: a.value as string };
          codeEmit(csid, { type: "config", models: null, modes: null, configOptions: fakeOptionsFor(csid) });
          return null;
        }
        case "list_agents": return FAKE_AGENTS;
        case "install_antigravity_agent": {
          const ag = FAKE_AGENTS.find((x) => x.id === "antigravity");
          if (ag) {
            ag.available = true;
            ag.program = "/opt/patchbay/agents/antigravity-acp/agy_acp_server.par";
            ag.version = "1.4.3";
            ag.source = "override";
            ag.error = null;
          }
          return "/opt/patchbay/agents/antigravity-acp/agy_acp_server.par";
        }
        case "host_info": return { version: "0.11.0", os: "macos", arch: "aarch64" };
        case "code_set_agent": {
          const s = db.code.find((x) => x.id === a.id)!;
          const saved = db.transcripts[s.id];
          if (s.agent !== a.agent && saved?.includes('"kind":"user"')) {
            // Like `handoff::fold_for_switch`: the old turns become earlier history.
            const v = JSON.parse(saved);
            const name = (id: string) => ({ patchbay: "Patchbay", grok: "Grok Build", "claude-code": "Claude Code" })[id] ?? id;
            const divider = `Switched from ${name(s.agent)} to ${name(a.agent as string)}. It got the conversation so far as context.`;
            db.transcripts[s.id] = JSON.stringify({
              v: 1,
              earlier: {
                entries: [...(v.earlier?.entries ?? []), ...(v.entries ?? []), { kind: "info", text: divider }],
                toolCalls: { ...v.earlier?.toolCalls, ...v.toolCalls },
              },
              entries: [],
              toolCalls: {},
            });
          }
          s.agent = a.agent as AgentId;
          s.agentModel = null;
          s.acpSessionId = null;
          return null;
        }
        case "code_rewind": await sleep(150); return null;
        case "code_usage": return { totals: { ...fakeUsage }, contextTokens: null, contextWindow: 128000, autoCompactPercent: 85 };
        case "plan_history": {
          // Bursts and lulls across the fake windows, matching `plan_limits` (not for `?plan=none|over`).
          const plan = new URLSearchParams(location.search).get("plan");
          if (plan === "none" || plan === "over") return [];
          const slow = plan === "slow";
          const five = fakePlanBase + 72 * 60;
          const seven = fakePlanBase + 3 * 86400 + 7200;
          const mix = (span: number, resetsAt: number, k: number, pts: [number, number][]) =>
            pts.map(([f, u]) => ({
              at: resetsAt - span + Math.round(f * span),
              fiveHour: k === 5 ? { utilization: slow ? u / 7 : u, resetsAt } : null,
              sevenDay: k === 7 ? { utilization: slow ? u / 4 : u, resetsAt } : null,
            }));
          return [
            ...mix(5 * 3600, five, 5, [[0.08, 0.05], [0.2, 0.12], [0.3, 0.14], [0.45, 0.4], [0.55, 0.55], [0.7, 0.6], [0.76, 0.82]]),
            ...mix(7 * 86400, seven, 7, [[0.1, 0.06], [0.25, 0.14], [0.4, 0.2], [0.5, 0.36], [0.57, 0.41]]),
          ].sort((a, b) => a.at - b.at);
        }
        case "plan_limits": {
          // `?plan=none` before any report; `?plan=over` past the limit and on extra usage; `?plan=slow` a light burn.
          const plan = new URLSearchParams(location.search).get("plan");
          if (plan === "none") return null;
          const now = Math.floor(Date.now() / 1000);
          const over = plan === "over";
          const slow = plan === "slow";
          return {
            fiveHour: { utilization: over ? 1 : slow ? 0.12 : 0.82, resetsAt: fakePlanBase + 72 * 60 },
            sevenDay: { utilization: over ? 0.96 : slow ? 0.1 : 0.41, resetsAt: fakePlanBase + 3 * 86400 + 7200 },
            overageUntil: over ? now + 72 * 60 : null,
            observedAt: now - 240,
          };
        }
        case "wire_log_status":
          return fakeWireStatus();
        case "wire_log_set":
          fakeWire.until = a.on ? Date.now() + 30 * 60_000 : null;
          if (a.on && fakeWire.frames.length === 0) {
            fakeWireRecord("claude-code", "out", { jsonrpc: "2.0", id: 0, method: "initialize", params: { protocolVersion: 1, _meta: { apiKey: "[redacted]" } } });
            fakeWireRecord("claude-code", "in", { jsonrpc: "2.0", id: 0, result: { protocolVersion: 1, agentInfo: { name: "claude-code-acp", version: "1.0" } } });
          }
          return fakeWireStatus();
        case "wire_log_frames":
          return fakeWire.frames.slice(-((a.last as number | null) ?? fakeWire.frames.length));
        case "wire_log_clear":
          fakeWire.frames = [];
          return fakeWireStatus();
        case "wire_log_save":
          return "/home/you/.local/share/patchbay/diagnostics/acp-wire-20261021-120000.jsonl";
        case "code_permission_respond": {
          const r = pending.get(a.requestId as string);
          pending.delete(a.requestId as string);
          r?.((a.optionId as string) ?? null);
          return null;
        }

        case "term_open": {
          if (a.authMethod) {
            // A terminal sign-in: the agent's login command prints a little and exits 0.
            const s = db.code.find((x) => x.id === a.sessionId);
            const m = s && FAKE_AUTH[s.agent]?.find((x) => x.id === a.authMethod && x.terminal);
            if (!s || !m) throw `the agent offers no terminal sign-in "${a.authMethod}"`;
            const out = (t: string, ms: number) => setTimeout(() => send<TermEvent>(a.onEvent, { type: "data", data: btoa(t) }), ms);
            out(`Opening browser to sign in (${m.name})...\r\n`, 100);
            out("Waiting for the browser... ", 600);
            out("\x1b[32mLogin successful.\x1b[0m\r\n", 2600);
            setTimeout(() => {
              fakeSignedIn.add(s.agent);
              send<TermEvent>(a.onEvent, { type: "exit", code: 0 });
            }, 3000);
            return null;
          }
          setTimeout(() => send<TermEvent>(a.onEvent, { type: "data", data: btoa(`demo@patchbay:${a.cwd}$ `) }), 50);
          (window as unknown as Record<string, unknown>).__term = a.onEvent;
          return null;
        }
        case "term_write": {
          const w = window as unknown as Record<string, unknown>;
          // Like a line-editing shell: echo printable keys, erase on backspace, and after
          // `sudo` read one line without echo. `__termLatency` (ms) delays the echo, as a
          // shell on another computer would.
          let out = "";
          for (const ch of a.data as string) {
            let line = (w.__termLine as string) ?? "";
            if (ch === "\r") {
              const cmd = line.trim();
              w.__termLine = "";
              if (w.__termSecret) {
                w.__termSecret = false;
                out += "\r\nok\r\ndemo@patchbay$ ";
              } else if (cmd === "exit") {
                out += "\r\n";
                setTimeout(() => send<TermEvent>(w.__term, { type: "exit", code: 0 }), Number(w.__termLatency ?? 0));
              } else if (cmd === "sudo") {
                w.__termSecret = true;
                out += "\r\n[sudo] password: ";
              } else {
                out += "\r\ndemo@patchbay$ ";
              }
              continue;
            }
            if (ch === "\x7f") {
              if (!line) continue;
              w.__termLine = line.slice(0, -1);
              if (!w.__termSecret) out += "\b\x1b[K";
              continue;
            }
            line += ch;
            w.__termLine = line;
            if (!w.__termSecret) out += ch;
          }
          if (out) setTimeout(() => send<TermEvent>(w.__term, { type: "data", data: btoa(out) }), Number(w.__termLatency ?? 0));
          return null;
        }
        case "term_resize": case "term_close": return null;

        case "code_attach": {
          const s = db.code.find((x) => x.id === a.id);
          if (s?.hostNode === "n-old") return { running: false, busy: false, events: [], offline: true };
          // An agent this fake started keeps running, so opening its session again just attaches.
          if (fakeRunning.has(a.id as string)) return { running: true, busy: false, events: [] };
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
          // On the phone, the requests waiting on the computers it is connected to.
          if (phoneApp) {
            phone.pending = phone.pending.filter((x) => x.nodeId !== a.nodeId);
            phoneChanged();
            return null;
          }
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
        case "mesh_set_passphrase": {
          if (((a.passphrase as string) ?? "").length < 8) throw "use a passphrase of at least 8 characters";
          mesh.problem = null;
          meshMember();
          meshStatusChanged();
          return null;
        }
        case "mesh_chat_streams": return [];

        case "remote_status": return structuredClone(phone);
        case "remote_join": {
          phone.joining = { url: `ws://${a.url as string}`, code: null, status: "connecting", error: null };
          phone.name = (a.name as string) || phone.name;
          phoneChanged();
          setTimeout(() => { phone.joining = { ...phone.joining!, status: "waiting", code: "305 118" }; phoneChanged(); }, 900);
          setTimeout(() => { phone.joining = null; phone.member = true; phoneChanged(); }, 4000);
          return null;
        }
        case "remote_cancel_join": phone.joining = null; phoneChanged(); return null;
        case "remote_leave": phone.member = false; phoneChanged(); return null;
        case "remote_unlock": case "remote_local_network": return true;
        case "remote_live": {
          const [working, waiting] = db.code;
          return {
            [working.id]: { status: "ready", busy: true, permissions: [], last: now() },
            [waiting.id]: { status: "ready", busy: false, permissions: ["r-demo"], last: now() },
          };
        }
        case "remote_kick": case "remote_set_foreground": case "remote_unwatch": case "remote_keep_alive": case "remote_take_join_action": return null;
        case "new_session_options":
        case "remote_new_session_options": {
          const agent = (id: AgentId, label: string, available: boolean) =>
            ({ id, label, available, program: null, source: null, version: null, error: null, installHint: "", usesPatchbayModels: id === "patchbay" || id === "grok", supportsInstructions: id === "patchbay" || id === "grok", takesProviders: false });
          return {
            agents: [agent("patchbay", "Patchbay", true), agent("grok", "Grok Build", true), agent("claude-code", "Claude Code", true), agent("cursor", "Cursor CLI", false)],
            defaultAgent: "patchbay",
            home: "/home/demo",
            recent: [...new Set(db.code.filter((s) => (s.hostNode ?? ME) === ((a.host as string | null) ?? ME)).map((s) => s.cwd))].slice(0, 12),
          };
        }
        case "list_dirs":
        case "remote_list_dirs": {
          const path = (a.path as string | null) ?? "/home/demo";
          const tree: Record<string, string[]> = {
            "/home/demo": ["Documents", "Downloads", "Projects"],
            "/home/demo/Projects": ["nulamp", "omnimark", "patchbay"],
          };
          if (!path.startsWith("/home/demo")) throw "only folders inside your home folder can be listed";
          return { path, parent: path === "/home/demo" ? null : path.replace(/\/[^/]+$/, ""), home: "/home/demo", dirs: tree[path] ?? [], truncated: false };
        }
        case "remote_create_code_session": {
          const s: CodeSession = {
            id: id(), cwd: a.cwd as string, acpSessionId: null, title: "New session", modelRef: (a.modelRef as string) ?? null,
            pinned: false, createdAt: now(), updatedAt: now(), activity: null, agent: (a.agent as AgentId) ?? "patchbay",
            agentModel: null, hostNode: a.host as string, epoch: 0, permissionMode: "default",
          };
          db.code.push(s);
          return s;
        }
      }
      console.warn("fake backend: unhandled", cmd, a);
      return null;
    },
    { shouldMockEvents: true },
  );
}
