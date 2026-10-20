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
  type AuthChoice,
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
import { isRemoteApp } from "./platform";
import { isForegroundUpdate } from "./codeWorking";

export { codeWorking, FOREGROUND_IDLE_MS, hasActiveForegroundTools } from "./codeWorking";

export type Mode = "chat" | "code";

export type Entry =
  | { kind: "user"; text: string; attachments?: Attachment[] }
  | { kind: "agent"; text: string }
  /** An image the agent sent in its reply (an ACP image block). */
  | { kind: "image"; data: string; mimeType: string }
  | { kind: "thought"; text: string }
  | { kind: "tool"; id: string }
  | { kind: "error"; text: string }
  | { kind: "info"; text: string };

export interface TranscriptPart {
  entries: Entry[];
  toolCalls: Record<string, AcpToolCall>;
}

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
  /** What the agent does without asking, whatever the session's approval mode. */
  approvalGaps: string[];
  /** The agent waits for a sign-in: before it starts a session, or mid-session. */
  auth: { message: string; methods: AuthChoice[] } | null;
  /** Tool call id -> why a saved rule or the approval mode approved it. */
  autoApproved: Record<string, string>;
  /** Cached transcript from a previous run, shown until (or unless) the agent replays it. */
  history: TranscriptPart | null;
  /** Turns run by agents the session had before its current one: shown, never replayed. */
  earlier: TranscriptPart | null;
  /** `agentConfigRev` this agent was started with; older ones restart when idle. */
  configRev: number;
  /** `catalogRev` this agent was started with, for agents that run on Patchbay's models. */
  catalogRev: number;
  /**
   * Last time a foreground session update arrived (agent text, thoughts, tools — not
   * background terminal streaming alone). Used with {@link codeWorking}.
   */
  lastForegroundAt: number | null;
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
  approvalGaps: [],
  auth: null,
  autoApproved: {},
  history: null,
  earlier: null,
  configRev: 0,
  catalogRev: 0,
  lastForegroundAt: null,
});

// The default permission mode isn't here: it only seeds new sessions, and agents always ask.
const AGENT_PREF_KEYS: (keyof Preferences)[] = [
  "agentInstructions",
  "webFetch",
  "autoCompactPercent",
  "backgroundSummaries",
  "claudeConfig",
  "agentPaths",
  "agentProviders",
];

/** Persist what the transcript shows, so the session renders before the agent replays it. */
function saveTranscript(id: string, s: CodeState) {
  if (s.entries.length === 0) return;
  const earlier = s.earlier ?? undefined;
  let json = JSON.stringify({ v: 1, earlier, entries: s.entries, toolCalls: s.toolCalls });
  if (json.length > 3_500_000) {
    // Too big: keep the conversation, drop tool payloads (diffs, outputs, images) and images.
    const slim = (calls: Record<string, AcpToolCall>) =>
      Object.fromEntries(
        Object.entries(calls).map(([k, tc]) => [k, { ...tc, content: null, rawInput: null, rawOutput: null }]),
      );
    const noImages = (entries: Entry[]): Entry[] =>
      entries.map((e) => (e.kind === "image" ? { kind: "info", text: "Image not kept: the saved transcript was too large." } : e));
    json = JSON.stringify({
      v: 1,
      earlier: earlier && { entries: noImages(earlier.entries), toolCalls: slim(earlier.toolCalls) },
      entries: noImages(s.entries),
      toolCalls: slim(s.toolCalls),
    });
  }
  api.saveCodeTranscript(id, json).catch(() => {});
}

/** The saved transcript: the current agent's part (`history`) and earlier agents' (`earlier`). */
async function loadTranscript(id: string): Promise<{ history: TranscriptPart | null; earlier: TranscriptPart | null }> {
  const part = (p?: Partial<TranscriptPart>): TranscriptPart | null =>
    p?.entries?.length ? { entries: p.entries, toolCalls: p.toolCalls ?? {} } : null;
  try {
    const raw = await api.getCodeTranscript(id);
    if (!raw) return { history: null, earlier: null };
    const v = JSON.parse(raw) as Partial<TranscriptPart> & { earlier?: Partial<TranscriptPart> };
    return { history: part(v), earlier: part(v.earlier) };
  } catch {
    return { history: null, earlier: null };
  }
}

/** Hide the context note Patchbay put before a prompt (see `handoff.rs`) when the agent replays it. */
function withoutNote(text: string): string {
  if (!text.startsWith("<patchbay-handoff>")) return text;
  const end = text.indexOf("</patchbay-handoff>");
  return end < 0 ? "" : text.slice(end + "</patchbay-handoff>".length).replace(/^\s+/, "");
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
      const { history, earlier } = await loadTranscript(s.id);
      const calls = [...Object.values(earlier?.toolCalls ?? {}), ...Object.values(history?.toolCalls ?? {})];
      recordActivity(s.id, history || earlier ? activityOf(calls) : "chat");
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
  /** A text field, e.g. for renaming; its value comes back in `text`. */
  input?: { value: string; label: string };
}

export interface ConfirmResult {
  ok: boolean;
  checked: boolean;
  text?: string;
}

export interface Alert {
  title: string;
  body: string;
  actions: { label: string; primary?: boolean; run: () => void }[];
  /** A mesh problem kind: the alert goes away once the mesh no longer reports it. */
  meshKind?: string;
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
  /** Bumped when Patchbay's providers or models actually change. */
  catalogRev: number;
  /** (Re)start the agent for a code session, showing the cached transcript meanwhile. */
  startAgent: (id: string) => Promise<void>;
  restartStaleAgents: () => void;
  settingsOpen: boolean;
  sidebarCollapsed: boolean;
  shortcutsOpen: boolean;
  /** The New session dialog (Code). */
  newSessionOpen: boolean;
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
  /** A problem toast: stays until dismissed or fixed. */
  alert: Alert | null;
  meshPopoverOpen: boolean;
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
  setNewSessionOpen: (o: boolean) => void;
  setSidebarQuery: (q: string) => void;
  setToast: (t: string | null) => void;
  setAlert: (a: Alert | null) => void;
  setMeshPopoverOpen: (o: boolean) => void;
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

type TerminalMeta = {
  terminal_info?: { terminal_id?: string };
  terminal_output?: { data?: string };
  terminal_output_delta?: { data?: string };
  terminal_exit?: { exit_code?: number | null };
};

function mergeToolCall(prev: AcpToolCall | undefined, upd: AcpToolCall): AcpToolCall {
  const out: AcpToolCall = { ...(prev ?? { toolCallId: upd.toolCallId }) };
  for (const [k, v] of Object.entries(upd)) {
    if (k === "sessionUpdate" || k === "_meta" || v === undefined || v === null) continue;
    (out as unknown as Record<string, unknown>)[k] = v;
  }
  // The terminal channel (`terminal_output` client capability, see acp.rs): output arrives in
  // chunks, the exit separately; a call that completed with its terminal still open was
  // backgrounded and runs on until its exit arrives.
  const meta = (upd as { _meta?: TerminalMeta | null })._meta;
  if (meta) {
    if (meta.terminal_info) out.terminal = true;
    const chunk = meta.terminal_output?.data ?? meta.terminal_output_delta?.data;
    if (chunk) out.terminalOutput = (out.terminalOutput ?? "") + chunk;
    if (meta.terminal_exit) {
      out.terminalExit = { exitCode: meta.terminal_exit.exit_code ?? null };
      out.background = false;
    }
  }
  if (out.terminal && !out.terminalExit && out.status === "completed") out.background = true;
  return out;
}

/** Tool calls with their backgrounded commands marked ended: the agent that ran them is gone. */
function endBackground(calls: Record<string, AcpToolCall>): Record<string, AcpToolCall> {
  if (!Object.values(calls).some((tc) => tc.background)) return calls;
  return Object.fromEntries(
    Object.entries(calls).map(([k, tc]) => [k, tc.background ? { ...tc, background: false } : tc]),
  );
}

function reduceUpdate(s: CodeState, u: AcpUpdate): CodeState {
  const stamp = (next: CodeState): CodeState => {
    const prev =
      u.sessionUpdate === "tool_call_update"
        ? s.toolCalls[(u as unknown as AcpToolCall).toolCallId]
        : undefined;
    return isForegroundUpdate(u, prev) ? { ...next, lastForegroundAt: Date.now() } : next;
  };
  switch (u.sessionUpdate) {
    case "user_message_chunk":
      // Live prompts are echoed locally; only replayed history (session/load) adds user entries.
      if (s.busy) return s;
      return {
        ...s,
        entries: appendText(s.entries, "user", withoutNote(textOf((u as unknown as { content: { type: string; text?: string } }).content))),
      };
    case "agent_message_chunk": {
      const c = (u as unknown as { content: { type: string; text?: string; data?: string; mimeType?: string } }).content;
      if (c?.type === "image" && c.data && c.mimeType) {
        return stamp({ ...s, entries: [...s.entries, { kind: "image", data: c.data, mimeType: c.mimeType }] });
      }
      return stamp({ ...s, entries: appendText(s.entries, "agent", textOf(c)) });
    }
    case "agent_thought_chunk":
      return stamp({
        ...s,
        entries: appendText(s.entries, "thought", textOf((u as unknown as { content: { type: string; text?: string } }).content)),
      });
    case "tool_call":
    case "tool_call_update": {
      const tc = u as unknown as AcpToolCall;
      const exists = !!s.toolCalls[tc.toolCallId];
      return stamp({
        ...s,
        toolCalls: { ...s.toolCalls, [tc.toolCallId]: mergeToolCall(s.toolCalls[tc.toolCallId], tc) },
        entries: exists ? s.entries : [...s.entries, { kind: "tool", id: tc.toolCallId }],
      });
    }
    case "plan":
      return stamp({ ...s, plan: (u as { entries: AcpPlanEntry[] }).entries });
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

/** The catalog last loaded, to tell a real change from a reload. */
let catalogKey: string | null = null;
/** Counts session-list loads, so only the latest one's answer is kept. */
let codeSessionsSeq = 0;

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
  catalogRev: 0,
  bumpAgentConfig: () => {
    set((st) => ({ agentConfigRev: st.agentConfigRev + 1 }));
    get().restartStaleAgents();
  },
  startAgent: async (id) => {
    const rev = get().agentConfigRev;
    const catalogRev = get().catalogRev;
    const prev = get().code[id];
    // Keep what is on screen as the placeholder; otherwise use the cached transcript.
    // A new agent process: whatever the old one ran in the background is gone.
    const shown = prev && prev.entries.length ? { entries: prev.entries, toolCalls: endBackground(prev.toolCalls) } : null;
    const earlier = prev?.earlier ?? null;
    set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), configRev: rev, catalogRev, history: shown, earlier } } }));
    if (!shown || !earlier) {
      loadTranscript(id).then((saved) => {
        const cur = get().code[id];
        if (!cur) return;
        const patch: Partial<CodeState> = {};
        if (!cur.earlier && saved.earlier) patch.earlier = saved.earlier;
        if (!cur.history && saved.history && cur.status !== "ready" && cur.entries.length === 0) {
          patch.history = { ...saved.history, toolCalls: endBackground(saved.history.toolCalls) };
        }
        if (Object.keys(patch).length) set((st) => ({ code: { ...st.code, [id]: { ...st.code[id], ...patch } } }));
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
    const { code, agentConfigRev, catalogRev, codeSessions, mesh, startAgent } = get();
    // Only agents running on this computer: a phone hosts none, and another computer's agent
    // follows that computer's settings, applied when it next starts there.
    if (isRemoteApp()) return;
    for (const [id, s] of Object.entries(code)) {
      const session = codeSessions.find((c) => c.id === id);
      if (!session || remoteHost(session, mesh)) continue;
      const stale = s.configRev < agentConfigRev || (!!s.extensions.patchbayModels && s.catalogRev < catalogRev);
      if (s.status === "ready" && !s.busy && s.permissions.length === 0 && stale) {
        startAgent(id).then(() =>
          get().pushCodeEntry(id, { kind: "info", text: "Restarted the agent to apply changed settings" }),
        );
      }
    }
  },
  settingsOpen: false,
  sidebarCollapsed: loadSidebarCollapsed(),
  shortcutsOpen: false,
  newSessionOpen: false,
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
  alert: null,
  meshPopoverOpen: false,
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
  setNewSessionOpen: (newSessionOpen) => set({ newSessionOpen }),
  setSidebarQuery: (sidebarQuery) => set({ sidebarQuery }),
  setToast: (toast) => set({ toast }),
  setAlert: (alert) => set({ alert }),
  setMeshPopoverOpen: (meshPopoverOpen) => set({ meshPopoverOpen }),

  refreshCatalog: async () => {
    const [providers, models, def] = await Promise.all([
      api.listProviders(),
      api.listModels(),
      api.getSetting("default_model"),
    ]);
    const defaultModelRef = def && models.some((m) => m.id === def) ? def : (models[0]?.id ?? null);
    set({ providers, models, defaultModelRef });
    // Agents built from the catalog restart when it really changed, not on every reload: a phone
    // reloads it after each reconnect.
    const key = JSON.stringify([providers, models]);
    if (catalogKey !== null && key !== catalogKey) {
      set((st) => ({ catalogRev: st.catalogRev + 1 }));
      get().restartStaleAgents();
    }
    catalogKey = key;
  },
  setDefaultModel: async (ref) => {
    await api.setSetting("default_model", ref);
    set({ defaultModelRef: ref });
  },

  refreshConversations: async () => set({ conversations: await api.listConversations() }),
  selectConversation: (id) => set({ activeConversationId: id }),

  refreshCodeSessions: async () => {
    // Calls can overlap (a sync event during a create): a slower, older answer must not win.
    const seq = ++codeSessionsSeq;
    const list = await api.listCodeSessions();
    if (seq !== codeSessionsSeq) return;
    set({ codeSessions: list });
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
    if (e.type === "title") {
      set((st) => ({
        codeSessions: st.codeSessions.map((x) => (x.id === id ? { ...x, title: e.title, titleManual: false } : x)),
      }));
      return;
    }
    const s = get().code[id] ?? emptyCode();
    let next: CodeState = s;
    switch (e.type) {
      case "status":
        next = {
          ...s,
          status: e.state,
          busy: e.state === "exited" ? false : s.busy,
          permissions: e.state === "exited" ? [] : s.permissions,
          auth: e.state === "exited" ? null : s.auth,
          // Commands an agent backgrounded end with it.
          toolCalls: e.state === "exited" ? endBackground(s.toolCalls) : s.toolCalls,
        };
        break;
      case "sessionReady": {
        // A replay rebuilt the transcript: drop the cached copy. No replay but resumed: the cache
        // is the transcript. A fresh session keeps the cache as earlier, out-of-context history.
        let { entries, toolCalls, history, earlier } = s;
        if (history && entries.length > 0) history = null;
        else if (history && e.loaded) {
          entries = history.entries;
          toolCalls = { ...history.toolCalls, ...toolCalls };
          history = null;
        } else if (history) {
          // Kept as earlier turns, so the next save doesn't drop them.
          earlier = {
            entries: [...(earlier?.entries ?? []), ...history.entries],
            toolCalls: { ...earlier?.toolCalls, ...history.toolCalls },
          };
          history = null;
        }
        next = {
          ...s,
          entries,
          toolCalls,
          history,
          earlier,
          models: e.models ?? null,
          modes: e.modes ?? null,
          capabilities: e.capabilities ?? null,
          configOptions: e.configOptions ?? [],
          extensions: e.extensions ?? { statusLine: false, rewind: false },
          approvalGaps: e.approvalGaps ?? [],
          auth: null,
        };
        break;
      }
      case "authRequired":
        next = { ...s, auth: { message: e.message, methods: e.methods } };
        notifyCode(id, "needs you to sign in");
        break;
      case "authResolved":
        next = { ...s, auth: null };
        break;
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
    // Both go to the gateway: ask at once rather than one round trip after the other.
    const attached = api.codeAttach(id);
    attached.catch(() => {});
    const { history, earlier } = await loadTranscript(id);
    let info;
    try {
      info = await attached;
    } catch (e) {
      set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), history, earlier, status: "exited" } } }));
      get().applyCodeEvent(id, { type: "error", message: errorText(e) });
      finish();
      return;
    }
    if (!info.running) {
      finish(info.point);
      if (info.offline) {
        set((st) => ({ code: { ...st.code, [id]: { ...emptyCode(), history, earlier, status: "exited" } } }));
        return;
      }
      await get().startAgent(id);
      return;
    }
    const rev = get().agentConfigRev;
    const catalogRev = get().catalogRev;
    set((st) => ({
      code: { ...st.code, [id]: { ...emptyCode(), configRev: rev, catalogRev, history, earlier, status: "ready", busy: info.busy } },
    }));
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
  // Phone soft keyboards: Enter inserts a line; send with the button (and Mod+Enter if a
  // hardware keyboard is attached). Desktop still follows the preference.
  if (isRemoteApp()) return e.ctrlKey || e.metaKey;
  return sendKey === "mod-enter" ? e.ctrlKey || e.metaKey : !e.shiftKey && !e.ctrlKey && !e.metaKey;
}

/**
 * True when Enter should insert a newline but the browser won't (Mod+Enter). Shift+Enter is
 * left to the textarea's default. Callers must preventDefault and insert `\n` themselves.
 */
export function needsExplicitNewline(
  e: { key: string; shiftKey: boolean; ctrlKey: boolean; metaKey: boolean },
  sendKey: Preferences["sendKey"],
): boolean {
  if (e.key !== "Enter" || !(e.ctrlKey || e.metaKey)) return false;
  return !isSendKey(e, sendKey);
}

/** Insert `\n` at the textarea caret and return the new value + caret. */
export function insertNewlineAtCaret(value: string, start: number, end: number): { value: string; caret: number } {
  return { value: value.slice(0, start) + "\n" + value.slice(end), caret: start + 1 };
}

export function modelLabel(models: Model[], providers: Provider[], ref: string | null): string {
  const m = models.find((x) => x.id === ref);
  if (!m) return "Select model";
  const p = providers.find((x) => x.id === m.providerId);
  return p ? `${m.displayName} · ${p.name}` : m.displayName;
}
