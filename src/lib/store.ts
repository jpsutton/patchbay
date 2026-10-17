import { create } from "zustand";
import {
  api,
  errorText,
  type AgentCapabilities,
  type AgentExtensions,
  type AcpCommand,
  type AcpConfigOption,
  type AcpModels,
  type AcpModes,
  type AcpPermissionRequest,
  type AcpPlanEntry,
  type AcpToolCall,
  type AcpUpdate,
  type CodeEvent,
  type CodeEnvelope,
  type EventPoint,
  type Attachment,
  type CodeSession,
  type Conversation,
  type Model,
  type Provider,
  type UsageView,
  type Preferences,
  type MeshStatus,
  type MeshChatEvent,
  defaultPreferences,
} from "./ipc";

import { notifyInBackground } from "./notify";
import { activityOf, strongest, type Activity } from "./activity";

export type Mode = "chat" | "code";

export type Entry =
  | { kind: "user"; text: string; attachments?: Attachment[] }
  | { kind: "agent"; text: string }
  | { kind: "thought"; text: string }
  | { kind: "tool"; id: string }
  | { kind: "error"; text: string }
  | { kind: "info"; text: string };

export interface CodeState {
  status: "idle" | "starting" | "ready" | "exited";
  busy: boolean;
  entries: Entry[];
  toolCalls: Record<string, AcpToolCall>;
  plan: AcpPlanEntry[] | null;
  models: AcpModels | null;
  modes: AcpModes | null;
  permissions: { requestId: string; request: AcpPermissionRequest }[];
  usage: UsageView | null;
  commands: AcpCommand[];
  capabilities: AgentCapabilities | null;
  /** Session config options besides model and mode (e.g. reasoning effort). */
  configOptions: AcpConfigOption[];
  /** Non-standard extensions the agent understands; gates rewind and the like. */
  extensions: AgentExtensions;
  /** Tool call id -> why a saved rule approved it. */
  autoApproved: Record<string, string>;
  /** Cached transcript from a previous run, shown until (or unless) the agent replays it. */
  history: { entries: Entry[]; toolCalls: Record<string, AcpToolCall> } | null;
  /** `agentConfigRev` this agent was started with; older ones restart when idle. */
  configRev: number;
}

export interface ChatStream {
  assistantId: string | null;
  text: string;
  reasoning: string;
  status: string | null;
  error: string | null;
  running: boolean;
  /** Name of the mesh node streaming it, when that isn't this one. */
  remote?: string | null;
}

const emptyCode = (): CodeState => ({
  status: "idle",
  busy: false,
  entries: [],
  toolCalls: {},
  plan: null,
  models: null,
  modes: null,
  permissions: [],
  usage: null,
  commands: [],
  capabilities: null,
  configOptions: [],
  extensions: { statusLine: false, rewind: false },
  autoApproved: {},
  history: null,
  configRev: 0,
});

const AGENT_PREF_KEYS: (keyof Preferences)[] = [
  "permissionMode",
  "agentInstructions",
  "webFetch",
  "autoCompactPercent",
  "backgroundSummaries",
  "claudeConfig",
  "agentPaths",
];

/** Persist what the transcript shows, so the session renders before the agent replays it. */
function saveTranscript(id: string, s: CodeState) {
  if (s.entries.length === 0) return;
  let json = JSON.stringify({ v: 1, entries: s.entries, toolCalls: s.toolCalls });
  if (json.length > 3_500_000) {
    // Too big: keep the conversation, drop tool payloads (diffs, outputs).
    const slim = Object.fromEntries(
      Object.entries(s.toolCalls).map(([k, tc]) => [k, { ...tc, content: null, rawInput: null, rawOutput: null }]),
    );
    json = JSON.stringify({ v: 1, entries: s.entries, toolCalls: slim });
  }
  api.saveCodeTranscript(id, json).catch(() => {});
}

async function loadTranscript(id: string): Promise<CodeState["history"]> {
  try {
    const raw = await api.getCodeTranscript(id);
    if (!raw) return null;
    const v = JSON.parse(raw) as { entries?: Entry[]; toolCalls?: Record<string, AcpToolCall> };
    return v.entries?.length ? { entries: v.entries, toolCalls: v.toolCalls ?? {} } : null;
  } catch {
    return null;
  }
}

/** Raise a session's stored activity (its sidebar icon) to `found`; it never goes back down. */
function recordActivity(id: string, found: Activity) {
  const st = useStore.getState();
  const sess = st.codeSessions.find((x) => x.id === id);
  if (!sess) return;
  const next = strongest(sess.activity, found)!;
  if (next === sess.activity) return;
  useStore.setState({ codeSessions: st.codeSessions.map((x) => (x.id === id ? { ...x, activity: next } : x)) });
  api.updateCodeSession(id, { activity: next }).catch(() => {});
}

let backfilling = false;

/** Sessions from before activity was tracked: work it out once from their saved transcripts. */
async function backfillActivity() {
  if (backfilling) return;
  backfilling = true;
  try {
    for (const s of useStore.getState().codeSessions.filter((x) => x.activity === null)) {
      const h = await loadTranscript(s.id);
      recordActivity(s.id, h ? activityOf(Object.values(h.toolCalls)) : "chat");
    }
  } finally {
    backfilling = false;
  }
}

export interface ConfirmRequest {
  title: string;
  body: string;
  confirmLabel?: string;
  danger?: boolean;
  checkbox?: { label: string; checked: boolean };
}

export interface ConfirmResult {
  ok: boolean;
  checked: boolean;
}

interface Store {
  mode: Mode;
  paletteOpen: boolean;
  setPaletteOpen: (o: boolean) => void;
  /** Section the next Settings open lands on (e.g. "usage"). */
  settingsSection: string | null;
  openSettingsAt: (section: string) => void;
  /** Scroll the chat to this message once it is loaded. */
  pendingJump: { conversationId: string; messageId: string } | null;
  setPendingJump: (j: Store["pendingJump"]) => void;
  /** Text from the quick-entry window, to send as a new chat. */
  pendingQuickPrompt: string | null;
  setPendingQuickPrompt: (t: string | null) => void;
  /** Per-conversation reasoning effort override for the next messages ("new" before one exists). */
  chatEffort: Record<string, string>;
  setChatEffort: (key: string, effort: string) => void;
  /** Bumped whenever settings that shape the agent change. */
  agentConfigRev: number;
  bumpAgentConfig: () => void;
  /** (Re)start the agent for a code session, showing the cached transcript meanwhile. */
  startAgent: (id: string) => Promise<void>;
  restartStaleAgents: () => void;
  settingsOpen: boolean;
  sidebarCollapsed: boolean;
  shortcutsOpen: boolean;
  /** Sidebar title filter. */
  sidebarQuery: string;
  providers: Provider[];
  models: Model[];
  defaultModelRef: string | null;
  conversations: Conversation[];
  activeConversationId: string | null;
  codeSessions: CodeSession[];
  activeCodeSessionId: string | null;
  code: Record<string, CodeState>;
  chatStreams: Record<string, ChatStream>;
  toast: string | null;
  prefs: Preferences;
  loadPrefs: () => Promise<void>;
  /** Persist a change; resolves to an error message when the backend rejects it. */
  updatePrefs: (patch: Partial<Preferences>) => Promise<string | null>;
  confirm: (ConfirmRequest & { resolve: (r: ConfirmResult) => void }) | null;

  askConfirm: (req: ConfirmRequest) => Promise<ConfirmResult>;
  truncateCode: (id: string, entryIndex: number) => void;
  setMode: (m: Mode) => void;
  setSettingsOpen: (o: boolean) => void;
  setSidebarCollapsed: (c: boolean) => void;
  setShortcutsOpen: (o: boolean) => void;
  setSidebarQuery: (q: string) => void;
  setToast: (t: string | null) => void;
  refreshCatalog: () => Promise<void>;
  setDefaultModel: (ref: string) => Promise<void>;
  refreshConversations: () => Promise<void>;
  selectConversation: (id: string | null) => void;
  refreshCodeSessions: () => Promise<void>;
  selectCodeSession: (id: string | null) => void;
  resetCode: (id: string) => void;
  pushCodeEntry: (id: string, e: Entry) => void;
  setCodeBusy: (id: string, busy: boolean) => void;
  setCodeUsage: (id: string, usage: UsageView) => void;
  applyCodeEvent: (id: string, e: CodeEvent) => void;
  patchChatStream: (convId: string, patch: Partial<ChatStream> | ((s: ChatStream) => Partial<ChatStream>)) => void;
  /** This node's mesh state (null until loaded). */
  mesh: MeshStatus | null;
  refreshMesh: () => Promise<void>;
  /** Bumped when another node's changes land, so open views reload. */
  syncRev: number;
  onSynced: (tables: string[]) => void;
  applyMeshChat: (p: MeshChatEvent) => void;
  /** Show a session that is already running (here, or on another node) without restarting it. */
  attachAgent: (id: string) => Promise<void>;
}

/** The node running a session's agent, when that isn't this one. */
export function remoteHost(s: CodeSession | null | undefined, mesh: MeshStatus | null): string | null {
  if (!s?.hostNode || !mesh || s.hostNode === mesh.nodeId) return null;
  return s.hostNode;
}

export function memberName(mesh: MeshStatus | null, nodeId: string | null | undefined): string {
  if (!nodeId) return "";
  return mesh?.members.find((m) => m.nodeId === nodeId)?.name ?? nodeId.slice(0, 8);
}

export function memberOnline(mesh: MeshStatus | null, nodeId: string | null | undefined): boolean {
  const st = mesh?.members.find((m) => m.nodeId === nodeId)?.state;
  return st === "direct" || st === "relay" || st === "self";
}

const emptyStream = (): ChatStream => ({
  assistantId: null,
  text: "",
  reasoning: "",
  status: null,
  error: null,
  running: false,
});

function textOf(content: { type: string; text?: string } | undefined): string {
  return content && content.type === "text" && typeof content.text === "string" ? content.text : "";
}

function appendText(entries: Entry[], kind: "agent" | "thought" | "user", text: string): Entry[] {
  if (!text) return entries;
  const last = entries[entries.length - 1];
  if (last && last.kind === kind) {
    return [...entries.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...entries, { kind, text } as Entry];
}

function mergeToolCall(prev: AcpToolCall | undefined, upd: AcpToolCall): AcpToolCall {
  const out: AcpToolCall = { ...(prev ?? { toolCallId: upd.toolCallId }) };
  for (const [k, v] of Object.entries(upd)) {
    if (k === "sessionUpdate" || v === undefined || v === null) continue;
    (out as unknown as Record<string, unknown>)[k] = v;
  }
  return out;
}

function reduceUpdate(s: CodeState, u: AcpUpdate): CodeState {
  switch (u.sessionUpdate) {
    case "user_message_chunk":
      // Live prompts are echoed locally; only replayed history (session/load) adds user entries.
      if (s.busy) return s;
      return { ...s, entries: appendText(s.entries, "user", textOf((u as unknown as { content: { type: string; text?: string } }).content)) };
    case "agent_message_chunk":
      return { ...s, entries: appendText(s.entries, "agent", textOf((u as unknown as { content: { type: string; text?: string } }).content)) };
    case "agent_thought_chunk":
      return { ...s, entries: appendText(s.entries, "thought", textOf((u as unknown as { content: { type: string; text?: string } }).content)) };
    case "tool_call":
    case "tool_call_update": {
      const tc = u as unknown as AcpToolCall;
      const exists = !!s.toolCalls[tc.toolCallId];
      return {
        ...s,
        toolCalls: { ...s.toolCalls, [tc.toolCallId]: mergeToolCall(s.toolCalls[tc.toolCallId], tc) },
        entries: exists ? s.entries : [...s.entries, { kind: "tool", id: tc.toolCallId }],
      };
    }
    case "plan":
      return { ...s, plan: (u as { entries: AcpPlanEntry[] }).entries };
    case "available_commands_update": {
      const cmds = (u as { availableCommands?: AcpCommand[] }).availableCommands;
      return Array.isArray(cmds) ? { ...s, commands: cmds } : s;
    }
    case "current_mode_update":
      return s.modes
        ? { ...s, modes: { ...s.modes, currentModeId: (u as { currentModeId: string }).currentModeId } }
        : s;
    default:
      return s;
  }
}

const SIDEBAR_KEY = "patchbay.sidebarCollapsed";

function loadSidebarCollapsed(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_KEY) === "1";
  } catch {
    return false;
  }
}

let catalogLoaded = false;

export const useStore = create<Store>((set, get) => ({
  mode: "chat",
  paletteOpen: false,
  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),
  settingsSection: null,
  openSettingsAt: (settingsSection) => set({ settingsSection, settingsOpen: true }),
  pendingJump: null,
  setPendingJump: (pendingJump) => set({ pendingJump }),
  pendingQuickPrompt: null,
  setPendingQuickPrompt: (pendingQuickPrompt) => set({ pendingQuickPrompt }),
  chatEffort: {},
  setChatEffort: (key, effort) => set((st) => ({ chatEffort: { ...st.chatEffort, [key]: effort } })),
  agentConfigRev: 0,
  bumpAgentConfig: () => {
    set((st) => ({ agentConfigRev: st.agentConfigRev + 1 }));
    get().restartStaleAgents();
  },
  startAgent: async (id) => {
    const rev = get().agentConfigRev;
    const prev = get().code[id];
    // Keep what is on screen as the placeholder; otherwise use the cached transcript.
    const shown = prev && prev.entries.length ? { entries: prev.entries, toolCalls: prev.toolCalls } : null;
    set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), configRev: rev, history: shown } } }));
    if (!shown) {
      loadTranscript(id).then((history) => {
        const cur = get().code[id];
        if (history && cur && cur.status !== "ready" && cur.entries.length === 0) {
          set((st) => ({ code: { ...st.code, [id]: { ...st.code[id], history } } }));
        }
      });
    }
    // Show stored totals right away; live updates arrive once the agent reports.
    api.codeUsage(id).then((u) => get().setCodeUsage(id, u)).catch(() => {});
    try {
      await api.codeStart(id);
    } catch (e) {
      get().applyCodeEvent(id, { type: "error", message: errorText(e) });
      get().applyCodeEvent(id, { type: "status", state: "exited", message: null });
    }
  },
  restartStaleAgents: () => {
    const { code, agentConfigRev, startAgent } = get();
    for (const [id, s] of Object.entries(code)) {
      if (s.status === "ready" && !s.busy && s.permissions.length === 0 && s.configRev < agentConfigRev) {
        startAgent(id).then(() =>
          get().pushCodeEntry(id, { kind: "info", text: "Restarted the agent to apply changed settings" }),
        );
      }
    }
  },
  settingsOpen: false,
  sidebarCollapsed: loadSidebarCollapsed(),
  shortcutsOpen: false,
  sidebarQuery: "",
  providers: [],
  models: [],
  defaultModelRef: null,
  conversations: [],
  activeConversationId: null,
  codeSessions: [],
  activeCodeSessionId: null,
  code: {},
  chatStreams: {},
  toast: null,
  confirm: null,
  prefs: defaultPreferences,
  loadPrefs: async () => {
    const loaded = await api.getPreferences();
    const prefs = {
      ...defaultPreferences,
      ...loaded,
      claudeConfig: { ...defaultPreferences.claudeConfig, ...loaded?.claudeConfig },
    };
    set({ prefs });
    applyUiPrefs(prefs);
  },
  updatePrefs: async (patch) => {
    const prev = get().prefs;
    const next = { ...prev, ...patch };
    try {
      await api.setPreferences(next);
    } catch (e) {
      return String(e);
    }
    set({ prefs: next });
    applyUiPrefs(next);
    if (AGENT_PREF_KEYS.some((k) => JSON.stringify(prev[k]) !== JSON.stringify(next[k]))) get().bumpAgentConfig();
    return null;
  },

  askConfirm: (req) =>
    new Promise<ConfirmResult>((resolve) =>
      set({
        confirm: {
          ...req,
          resolve: (r) => {
            set({ confirm: null });
            resolve(r);
          },
        },
      }),
    ),
  truncateCode: (id, entryIndex) =>
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, entries: s.entries.slice(0, entryIndex), plan: null } } };
    }),

  setMode: (mode) => set({ mode }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
  setSidebarCollapsed: (sidebarCollapsed) => {
    try {
      localStorage.setItem(SIDEBAR_KEY, sidebarCollapsed ? "1" : "0");
    } catch {
      /* storage unavailable */
    }
    set({ sidebarCollapsed });
  },
  setShortcutsOpen: (shortcutsOpen) => set({ shortcutsOpen }),
  setSidebarQuery: (sidebarQuery) => set({ sidebarQuery }),
  setToast: (toast) => set({ toast }),

  refreshCatalog: async () => {
    const [providers, models, def] = await Promise.all([
      api.listProviders(),
      api.listModels(),
      api.getSetting("default_model"),
    ]);
    const defaultModelRef = def && models.some((m) => m.id === def) ? def : (models[0]?.id ?? null);
    set({ providers, models, defaultModelRef });
    // Later reloads follow provider/model edits, which the agent config is generated from.
    if (catalogLoaded) get().bumpAgentConfig();
    catalogLoaded = true;
  },
  setDefaultModel: async (ref) => {
    await api.setSetting("default_model", ref);
    set({ defaultModelRef: ref });
  },

  refreshConversations: async () => set({ conversations: await api.listConversations() }),
  selectConversation: (id) => set({ activeConversationId: id }),

  refreshCodeSessions: async () => {
    set({ codeSessions: await api.listCodeSessions() });
    backfillActivity();
  },
  selectCodeSession: (id) => set({ activeCodeSessionId: id }),

  resetCode: (id) => set((st) => ({ code: { ...st.code, [id]: emptyCode() } })),
  pushCodeEntry: (id, e) =>
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, entries: [...s.entries, e] } } };
    }),
  setCodeBusy: (id, busy) =>
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, busy } } };
    }),

  setCodeUsage: (id, usage) =>
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, usage } } };
    }),

  applyCodeEvent: (id, e) => {
    const s = get().code[id] ?? emptyCode();
    let next: CodeState = s;
    switch (e.type) {
      case "status":
        next = {
          ...s,
          status: e.state,
          busy: e.state === "exited" ? false : s.busy,
          permissions: e.state === "exited" ? [] : s.permissions,
        };
        break;
      case "sessionReady": {
        // A replay rebuilt the transcript: drop the cached copy. No replay but resumed: the cache
        // is the transcript. A fresh session keeps the cache as earlier, out-of-context history.
        let { entries, toolCalls, history } = s;
        if (history && entries.length > 0) history = null;
        else if (history && e.loaded) {
          entries = history.entries;
          toolCalls = { ...history.toolCalls, ...toolCalls };
          history = null;
        }
        next = {
          ...s,
          entries,
          toolCalls,
          history,
          models: e.models ?? null,
          modes: e.modes ?? null,
          capabilities: e.capabilities ?? null,
          configOptions: e.configOptions ?? [],
          extensions: e.extensions ?? { statusLine: false, rewind: false },
        };
        break;
      }
      case "config":
        // Options-based agents resend their full config; legacy model/mode lists stay as they are.
        next = {
          ...s,
          models: e.models ?? s.models,
          modes: e.modes ?? s.modes,
          configOptions: e.configOptions ?? [],
        };
        break;
      case "autoApproved":
        next = { ...s, autoApproved: { ...s.autoApproved, [e.toolCallId]: e.description } };
        break;
      case "notice":
        next = { ...s, entries: [...s.entries, { kind: "info", text: e.message }] };
        break;
      case "update":
        next = reduceUpdate(s, e.update);
        break;
      case "permission":
        next = { ...s, permissions: [...s.permissions, { requestId: e.requestId, request: e.request }] };
        notifyCode(id, "needs your approval");
        break;
      case "permissionResolved":
        next = { ...s, permissions: s.permissions.filter((p) => p.requestId !== e.requestId) };
        break;
      case "promptDone":
        next = {
          ...s,
          busy: false,
          entries:
            e.stopReason === "cancelled" ? [...s.entries, { kind: "info", text: "Cancelled" }] : s.entries,
        };
        saveTranscript(id, next);
        {
          const calls = [...Object.values(next.history?.toolCalls ?? {}), ...Object.values(next.toolCalls)];
          queueMicrotask(() => recordActivity(id, activityOf(calls)));
        }
        if (e.stopReason !== "cancelled") notifyCode(id, "finished");
        break;
      case "error":
        next = { ...s, entries: [...s.entries, { kind: "error", text: e.message }] };
        break;
      case "usage":
        next = { ...s, usage: e.usage };
        break;
      case "userPrompt":
        // The window that sent it already shows it, unless this is a catch-up after reconnecting.
        if (e.origin === get().mesh?.nodeId && !e.replay) return;
        next = {
          ...s,
          busy: true,
          entries: [...s.entries, { kind: "user", text: e.text, attachments: e.attachments?.length ? e.attachments : undefined }],
        };
        break;
    }
    set((st) => ({ code: { ...st.code, [id]: next } }));
    if (e.type === "promptDone" || e.type === "permissionResolved") get().restartStaleAgents();
  },

  patchChatStream: (convId, patch) =>
    set((st) => {
      const cur = st.chatStreams[convId] ?? emptyStream();
      const p = typeof patch === "function" ? patch(cur) : patch;
      return { chatStreams: { ...st.chatStreams, [convId]: { ...cur, ...p } } };
    }),

  mesh: null,
  refreshMesh: async () => {
    try {
      set({ mesh: await api.meshStatus() });
    } catch {
      /* older backend */
    }
  },
  syncRev: 0,
  onSynced: (tables) => {
    set((st) => ({ syncRev: st.syncRev + 1 }));
    const has = (...t: string[]) => t.some((x) => tables.includes(x));
    if (has("conversations", "messages")) get().refreshConversations().catch(() => {});
    if (has("code_sessions")) get().refreshCodeSessions().catch(() => {});
    if (has("providers", "models", "mesh_secrets")) get().refreshCatalog().catch(() => {});
    if (has("mesh_members")) get().refreshMesh();
  },
  applyMeshChat: (p) => {
    if (p.streamer === get().mesh?.nodeId) return;
    const e = p.event;
    const conv = p.conversationId;
    const patch = get().patchChatStream;
    switch (e.type) {
      case "started":
      case "snapshot":
        patch(conv, {
          assistantId: e.assistantMessageId,
          text: e.type === "snapshot" ? e.text : "",
          reasoning: e.type === "snapshot" ? e.reasoning : "",
          status: null,
          error: null,
          running: true,
          remote: p.streamerName,
        });
        break;
      case "text":
        patch(conv, (s) => ({ text: s.text + e.text, status: null }));
        break;
      case "reasoning":
        patch(conv, (s) => ({ reasoning: s.reasoning + e.text }));
        break;
      case "status":
        patch(conv, { status: e.message });
        break;
      case "error":
        patch(conv, { error: e.message, running: false });
        break;
      case "done":
        patch(conv, { running: false });
        // The finished reply arrives through sync a moment later.
        setTimeout(() => get().onSynced(["messages"]), 400);
        break;
    }
  },
  attachAgent: async (id) => {
    // Live events that arrive while the catch-up is in flight wait, so none is lost or doubled.
    attaching.set(id, []);
    const finish = (point?: EventPoint | null) => {
      const held = attaching.get(id) ?? [];
      attaching.delete(id);
      for (const { e, pt } of held) {
        if (point && pt && pt.s === point.s && pt.n <= point.n) continue;
        get().applyCodeEvent(id, e);
      }
    };
    const history = await loadTranscript(id);
    let info;
    try {
      info = await api.codeAttach(id);
    } catch (e) {
      set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), history, status: "exited" } } }));
      get().applyCodeEvent(id, { type: "error", message: errorText(e) });
      finish();
      return;
    }
    if (!info.running) {
      finish(info.point);
      if (info.offline) {
        set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), history, status: "exited" } } }));
        return;
      }
      await get().startAgent(id);
      return;
    }
    const rev = get().agentConfigRev;
    set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), configRev: rev, history, status: "ready", busy: info.busy } } }));
    for (const e of info.events) get().applyCodeEvent(id, e);
    // A replayed "ready" counts as loaded, so the cached transcript becomes the session's.
    const cur = get().code[id];
    if (cur?.history && cur.entries.length === 0) {
      set((st) => ({
        code: { ...st.code, [id]: { ...cur, entries: cur.history!.entries, toolCalls: cur.history!.toolCalls, history: null } },
      }));
    }
    finish(info.point);
  },
}));

/** Sessions mid-catch-up, with the live events held back meanwhile. */
const attaching = new Map<string, { e: CodeEvent; pt?: EventPoint }[]>();

/** A live Code event from the backend: applied now, or after the catch-up in progress. */
export function codeEventArrived(env: CodeEnvelope) {
  const held = attaching.get(env.codeSessionId);
  if (held) held.push({ e: env.event, pt: env._pt });
  else useStore.getState().applyCodeEvent(env.codeSessionId, env.event);
}

function notifyCode(id: string, what: string) {
  const title = useStore.getState().codeSessions.find((c) => c.id === id)?.title ?? "Code session";
  notifyInBackground(`Agent ${what}`, title);
}

const TEXT_SIZE_PX = { small: "15px", default: "16px", large: "17.5px" } as const;

/** Apply preferences that live in the DOM (theme, text size). */
export function applyUiPrefs(p: Preferences) {
  const root = document.documentElement;
  if (p.theme === "system") delete root.dataset.theme;
  else root.dataset.theme = p.theme;
  root.style.fontSize = TEXT_SIZE_PX[p.textSize] ?? TEXT_SIZE_PX.default;
}

/** True when this keydown should send, per the "send with" preference. */
export function isSendKey(e: { key: string; shiftKey: boolean; ctrlKey: boolean; metaKey: boolean }, sendKey: Preferences["sendKey"]) {
  if (e.key !== "Enter") return false;
  return sendKey === "mod-enter" ? e.ctrlKey || e.metaKey : !e.shiftKey && !e.ctrlKey && !e.metaKey;
}

export function modelLabel(models: Model[], providers: Provider[], ref: string | null): string {
  const m = models.find((x) => x.id === ref);
  if (!m) return "Select model";
  const p = providers.find((x) => x.id === m.providerId);
  return p ? `${m.displayName} · ${p.name}` : m.displayName;
}
