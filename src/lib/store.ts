import { create } from "zustand";
import {
  api,
  errorText,
  type AgentCapabilities,
  type AgentExtensions,
  type AgentId,
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
  type CodeResume,
  type EventPoint,
  type Attachment,
  type CodeSession,
  type Conversation,
  type Model,
  type Provider,
  type Subagent,
  type UsageView,
  type PlanAccount,
  type Preferences,
  type MeshStatus,
  type MeshChatEvent,
  defaultPreferences,
} from "./ipc";

import { notifyInBackground } from "./notify";
import { activityOf, strongest, type Activity } from "./activity";
import { isRemoteApp } from "./platform";
import { isForegroundUpdate } from "./codeWorking";
import { chunkChars, phaseOf, type Phase } from "./workingStatus";
import { appendUserChunk } from "./harness";
import { planUpdateOf } from "./todoPlan";
import { applyChange, type LayoutChange, type SidebarEntry } from "./sidebarLayout";

export { codeWorking, FOREGROUND_IDLE_MS, hasActiveForegroundTools } from "./codeWorking";
export { harnessTags } from "./harness";

export type Mode = "chat" | "code";

export type Entry =
  /** `promptId`: names the prompt in its `userPrompt` events, so each shows once. `pending`: steered
   *  into the running turn, and the agent has not taken a step since (see `readSteers`). */
  | { kind: "user"; text: string; attachments?: Attachment[]; promptId?: string; at?: number; pending?: boolean }
  | { kind: "agent"; text: string; at?: number }
  /** An image the agent sent in its reply (an ACP image block). */
  | { kind: "image"; data: string; mimeType: string; from?: "user" }
  | { kind: "thought"; text: string }
  | { kind: "tool"; id: string }
  | { kind: "error"; text: string }
  | { kind: "info"; text: string }
  /** A replayed user-role message the agent's harness wrote (a reminder, a slash command's output). */
  | { kind: "harness"; text: string }
  /** A replayed marker that the user stopped the turn. */
  | { kind: "cancelled" }
  /** Something the agent sent that Patchbay can't show, with its raw payload as capped JSON
   *  text (see `rawText`); `count` times this turn. */
  | { kind: "unrecognized"; text: string; raw: string; count?: number };

/** A prompt waiting for the running turn to end (agents that can't take one mid-turn). */
export interface QueuedPrompt {
  promptId: string;
  text: string;
  attachments?: Attachment[];
  /** The mesh node that sent it. */
  origin: string;
  /** Learned from a catch-up, not live: never handed back to a composer. */
  replayed?: boolean;
}

/** What a composer gets back when queued prompts it sent won't be. */
export interface ComposerRestore {
  text: string;
  attachments: Attachment[];
}

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
  /** The agent's own id for this session, from its last `sessionReady` (the saved one can lag). */
  acpSessionId: string | null;
  /** Who the agent says it is (name, version), from `initialize`. */
  agentInfo: { name?: string; title?: string; version?: string } | null;
  /** Session config options besides model and mode (e.g. reasoning effort). */
  configOptions: AcpConfigOption[];
  /** Non-standard extensions the agent understands; gates rewind and the like. */
  extensions: AgentExtensions;
  /** What the agent does without asking, whatever the session's approval mode. */
  approvalGaps: string[];
  /** The agent waits for a sign-in: before it starts a session, or mid-session. */
  auth: { message: string; methods: AuthChoice[] } | null;
  /** Prompts waiting for the running turn to end. */
  queued: QueuedPrompt[];
  /** Queued prompts this window sent that a stop dropped, for its composer to take back. */
  restore: ComposerRestore | null;
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
  /** What the agent last did in the foreground; with {@link lastForegroundAt}, says what it is doing. */
  lastPhase: Phase | null;
  /** When the running prompt turn began; null between turns. */
  turnStartedAt: number | null;
  /** Characters of text and thoughts streamed this turn, for a rough token count. */
  turnChars: number;
  /** Patchbay's agent: its subagents, as of the last `subagents_update`. */
  subagents: Subagent[];
  /** When that update arrived, to count idle time on from it. */
  subagentsAt: number | null;
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
  acpSessionId: null,
  agentInfo: null,
  configOptions: [],
  extensions: { statusLine: false, rewind: false },
  approvalGaps: [],
  auth: null,
  queued: [],
  restore: null,
  autoApproved: {},
  history: null,
  earlier: null,
  configRev: 0,
  catalogRev: 0,
  lastForegroundAt: null,
  lastPhase: null,
  turnStartedAt: null,
  turnChars: 0,
  subagents: [],
  subagentsAt: null,
});

/** `next` with the turn clock started or cleared when `busy` flipped between `prev` and it. */
function trackTurn(prev: CodeState, next: CodeState): CodeState {
  if (prev.busy === next.busy) return next;
  return next.busy
    ? { ...next, turnStartedAt: Date.now(), turnChars: 0, lastPhase: null }
    : { ...next, turnStartedAt: null, turnChars: 0 };
}

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

/** Saved preferences with defaults filled in for anything an older backend leaves out. */
function withDefaults(loaded: Partial<Preferences> | null | undefined): Preferences {
  return {
    ...defaultPreferences,
    ...loaded,
    claudeConfig: { ...defaultPreferences.claudeConfig, ...loaded?.claudeConfig },
    usageLimit: { ...defaultPreferences.usageLimit, ...loaded?.usageLimit },
    agentEngine: { ...defaultPreferences.agentEngine, ...loaded?.agentEngine },
  };
}

/** Show and apply saved preferences; agents restart when a setting they run with changed. */
function setPrefs(next: Preferences) {
  const prev = useStore.getState().prefs;
  useStore.setState({ prefs: next });
  applyUiPrefs(next);
  if (AGENT_PREF_KEYS.some((k) => JSON.stringify(prev[k]) !== JSON.stringify(next[k]))) {
    useStore.getState().bumpAgentConfig();
  }
}

/** Add the queued prompts `me` sent (live, not from a catch-up) to what its composer takes back. */
function handBack(prev: ComposerRestore | null, dropped: QueuedPrompt[], me: string | undefined): ComposerRestore | null {
  const mine = dropped.filter((q) => q.origin === me && !q.replayed);
  if (mine.length === 0) return prev;
  return {
    text: [prev?.text, ...mine.map((q) => q.text)].filter((t) => t?.trim()).join("\n\n"),
    attachments: [...(prev?.attachments ?? []), ...mine.flatMap((q) => q.attachments ?? [])],
  };
}

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
  importClaudeOpen: boolean;
  /** The Code session whose details dialog is open (see `code/SessionInfoDialog.tsx`). */
  sessionInfoId: string | null;
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
  /** Each Claude account's last reported plan usage; empty until a turn on a Claude plan reports one. */
  planAccounts: PlanAccount[];
  planOpen: boolean;
  prefs: Preferences;
  loadPrefs: () => Promise<void>;
  /** Preferences someone saved on this backend (another window or browser tab): show them here. */
  prefsSaved: (saved: Preferences) => void;
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
  setImportClaudeOpen: (o: boolean) => void;
  setSessionInfoId: (id: string | null) => void;
  setSidebarQuery: (q: string) => void;
  setToast: (t: string | null) => void;
  setAlert: (a: Alert | null) => void;
  setMeshPopoverOpen: (o: boolean) => void;
  setPlanAccounts: (a: PlanAccount[]) => void;
  setPlanOpen: (o: boolean) => void;
  refreshCatalog: () => Promise<void>;
  setDefaultModel: (ref: string) => Promise<void>;
  refreshConversations: () => Promise<void>;
  selectConversation: (id: string | null) => void;
  refreshCodeSessions: () => Promise<void>;
  /** The Code sidebar's groups and folder order, synced across the mesh. Null until first loaded. */
  sidebarLayout: SidebarEntry[] | null;
  refreshSidebarLayout: () => Promise<void>;
  /** Show a layout change at once, then write it; reload if the write fails. */
  changeSidebarLayout: (change: LayoutChange) => Promise<void>;
  selectCodeSession: (id: string | null) => void;
  resetCode: (id: string) => void;
  pushCodeEntry: (id: string, e: Entry) => void;
  setCodeBusy: (id: string, busy: boolean) => void;
  /** The composer took back `restore`. */
  clearCodeRestore: (id: string) => void;
  setCodeUsage: (id: string, usage: UsageView) => void;
  applyCodeEvent: (id: string, e: CodeEvent) => void;
  /** Applies events in order with one store update, so a replay doesn't re-render per event. */
  applyCodeEvents: (id: string, events: CodeEvent[]) => void;
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
  /** Apply a `code_resume` snapshot: the turn's state and the end of calls no longer running. */
  applyCodeResume: (id: string, r: Extract<CodeResume, { mode: "snapshot" }>) => void;
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

/** Most characters of an unrecognized payload kept (and shown). */
const MAX_RAW = 20_000;

/** An unrecognized payload as indented JSON, cut to `MAX_RAW` characters. */
export function rawText(raw: unknown): string {
  const json = JSON.stringify(raw, null, 2) ?? "null";
  return json.length > MAX_RAW ? `${json.slice(0, MAX_RAW)}\n…[${json.length - MAX_RAW} more characters]` : json;
}

function appendText(entries: Entry[], kind: "agent" | "thought" | "user", text: string, at?: number): Entry[] {
  if (!text) return entries;
  const last = entries[entries.length - 1];
  if (last && last.kind === kind) {
    return [...entries.slice(0, -1), { ...last, text: last.text + text }];
  }
  if ((kind === "agent" || kind === "user") && at) return [...entries, { kind, text, at }];
  return [...entries, { kind, text } as Entry];
}

type TerminalMeta = {
  terminal_info?: { terminal_id?: string };
  terminal_output?: { data?: string };
  terminal_output_delta?: { data?: string };
  terminal_exit?: { exit_code?: number | null };
  /** Patchbay's own agent: a call started a background job; `done` when it ended. */
  background_shell?: { id?: string; done?: boolean; exit_code?: number | null };
};

export function mergeToolCall(prev: AcpToolCall | undefined, upd: AcpToolCall): AcpToolCall {
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
    const job = meta.background_shell;
    if (job?.id) {
      // The job can end before the call's own update lands: once done, it stays done.
      const done = !!job.done || (out.shell?.id === job.id && out.shell.done);
      out.shell = { id: job.id, done, exitCode: job.exit_code ?? out.shell?.exitCode };
      out.background = !done;
    }
  }
  if (out.terminal && !out.terminalExit && out.status === "completed") out.background = true;
  return out;
}

/** Tool calls with their backgrounded commands marked ended: the agent that ran them is gone. */
function endBackground(calls: Record<string, AcpToolCall>): Record<string, AcpToolCall> {
  if (!Object.values(calls).some((tc) => tc.background)) return calls;
  return Object.fromEntries(
    Object.entries(calls).map(([k, tc]) => [
      k,
      tc.background ? { ...tc, background: false, shell: tc.shell && { ...tc.shell, done: true } } : tc,
    ]),
  );
}

/** `entries` with no steered message waiting on the agent: its turn is over. */
function noPending(entries: Entry[]): Entry[] {
  return entries.some((x) => x.kind === "user" && x.pending)
    ? entries.map((x) => (x.kind === "user" && x.pending ? { ...x, pending: false } : x))
    : entries;
}

/**
 * Steered messages the agent has probably read: it takes one in before its next model request, so
 * once a tool call after the message has finished and the agent goes on, it is no longer waiting.
 * ACP says nothing when a steer is read, so this is a guess from the transcript.
 */
function readSteers(s: CodeState): CodeState {
  if (!s.entries.some((x) => x.kind === "user" && x.pending)) return s;
  let stepped = false;
  let entries = s.entries;
  for (let i = entries.length - 1; i >= 0; i--) {
    const x = entries[i];
    if (x.kind === "tool") {
      const status = s.toolCalls[x.id]?.status;
      if (status === "completed" || status === "failed") stepped = true;
    } else if (x.kind === "user" && x.pending && stepped) {
      if (entries === s.entries) entries = [...entries];
      entries[i] = { ...x, pending: false };
    }
  }
  return entries === s.entries ? s : { ...s, entries };
}

function reduceUpdate(s: CodeState, u: AcpUpdate, agent: AgentId | undefined): CodeState {
  const stamp = (next: CodeState): CodeState => {
    const prev =
      u.sessionUpdate === "tool_call_update"
        ? s.toolCalls[(u as unknown as AcpToolCall).toolCallId]
        : undefined;
    if (!isForegroundUpdate(u, prev)) return next;
    return {
      ...next,
      lastForegroundAt: Date.now(),
      lastPhase: phaseOf(u) ?? next.lastPhase,
      turnChars: next.busy ? next.turnChars + chunkChars(u) : next.turnChars,
    };
  };
  switch (u.sessionUpdate) {
    case "user_message_chunk": {
      // Live prompts are echoed locally; only replayed history (session/load) adds user entries.
      if (s.busy) return s;
      const c = (u as unknown as { content: { type: string; text?: string; data?: string; mimeType?: string } }).content;
      if (c?.type === "image" && c.data && c.mimeType) {
        return { ...s, entries: [...s.entries, { kind: "image", data: c.data, mimeType: c.mimeType, from: "user" }] };
      }
      return { ...s, entries: appendUserChunk(s.entries, withoutNote(textOf(c))) };
    }
    case "agent_message_chunk": {
      const c = (u as unknown as { content: { type: string; text?: string; data?: string; mimeType?: string } }).content;
      if (c?.type === "image" && c.data && c.mimeType) {
        return stamp({ ...s, entries: [...s.entries, { kind: "image", data: c.data, mimeType: c.mimeType }] });
      }
      return stamp({ ...s, entries: appendText(s.entries, "agent", textOf(c), s.status === "ready" ? Date.now() : undefined) });
    }
    case "agent_thought_chunk":
      return stamp({
        ...s,
        entries: appendText(s.entries, "thought", textOf((u as unknown as { content: { type: string; text?: string } }).content)),
      });
    case "tool_call":
    case "tool_call_update": {
      let tc = u as unknown as AcpToolCall;
      const prev = s.toolCalls[tc.toolCallId];
      const exists = !!prev;
      // ACP defaults for a new call's announcement (the backend fills them too). In an update, or
      // a re-announcement of a known call, an absent field means unchanged.
      if (u.sessionUpdate === "tool_call" && !exists) tc = { ...tc, status: tc.status ?? "pending", kind: tc.kind ?? "other" };
      const merged = mergeToolCall(prev, tc);
      // OpenCode's and Kilo's to-do writes are their plan; the transcript hides their cards.
      const todos = planUpdateOf(prev, merged, agent);
      return stamp({
        ...s,
        toolCalls: { ...s.toolCalls, [tc.toolCallId]: merged },
        entries: exists ? s.entries : [...s.entries, { kind: "tool", id: tc.toolCallId }],
        plan: todos ?? s.plan,
      });
    }
    case "plan":
      return stamp({ ...s, plan: (u as { entries: AcpPlanEntry[] }).entries });
    case "subagents_update": {
      const agents = (u as { agents?: Subagent[] }).agents;
      return Array.isArray(agents) ? { ...s, subagents: agents, subagentsAt: Date.now() } : s;
    }
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

/** One Code event applied to a session's state; notifications and saves happen here too. */
function stepCodeEvent(id: string, s: CodeState, e: Exclude<CodeEvent, { type: "title" }>): CodeState {
  let next: CodeState = s;
  switch (e.type) {
    case "status":
      next = {
        ...s,
        status: e.state,
        busy: e.state === "exited" ? false : s.busy,
        entries: e.state === "exited" ? noPending(s.entries) : s.entries,
        permissions: e.state === "exited" ? [] : s.permissions,
        auth: e.state === "exited" ? null : s.auth,
        // The agent's queue goes with it; this window gets back what it wrote.
        queued: e.state === "exited" ? [] : s.queued,
        restore: e.state === "exited" ? handBack(s.restore, s.queued, useStore.getState().mesh?.nodeId) : s.restore,
        // Commands an agent backgrounded end with it, and so do its subagents.
        toolCalls: e.state === "exited" ? endBackground(s.toolCalls) : s.toolCalls,
        subagents: e.state === "exited" ? [] : s.subagents,
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
        acpSessionId: e.acpSessionId ?? null,
        agentInfo: e.agentInfo ?? null,
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
    case "unrecognized": {
      // Repeats of the same kind in a turn (an unknown chunk type, say) show as one line, with
      // the latest payload, even with other entries between them.
      const raw = rawText(e.raw);
      let at = -1;
      for (let i = s.entries.length - 1; i >= 0; i--) {
        const x = s.entries[i];
        if (x.kind === "user") break;
        if (x.kind === "unrecognized" && x.text === e.message) {
          at = i;
          break;
        }
      }
      if (at >= 0) {
        const prev = s.entries[at] as Extract<Entry, { kind: "unrecognized" }>;
        const entries = [...s.entries];
        entries[at] = { ...prev, raw, count: (prev.count ?? 1) + 1 };
        next = { ...s, entries };
      } else {
        next = { ...s, entries: [...s.entries, { kind: "unrecognized", text: e.message, raw }] };
      }
      break;
    }
    case "update": {
      next = reduceUpdate(s, e.update, useStore.getState().codeSessions.find((x) => x.id === id)?.agent);
      const u = e.update;
      // The agent speaks or starts a tool: a step it took after a steered message came in.
      if (
        u.sessionUpdate === "agent_message_chunk" ||
        u.sessionUpdate === "agent_thought_chunk" ||
        (u.sessionUpdate === "tool_call" && !s.toolCalls[(u as unknown as AcpToolCall).toolCallId])
      ) {
        next = readSteers(next);
      }
      break;
    }
    case "permission": {
      // The request may carry fields no update did (a diff, rawInput): keep them with the call
      // so they outlast the request and reach the Changes panel. A call no update announced yet
      // gets its transcript entry here, since the later announcement finds it known.
      const tc = e.request.toolCall;
      const known = !!s.toolCalls[tc.toolCallId];
      next = {
        ...s,
        toolCalls: { ...s.toolCalls, [tc.toolCallId]: mergeToolCall(s.toolCalls[tc.toolCallId], tc) },
        entries: known ? s.entries : [...s.entries, { kind: "tool", id: tc.toolCallId }],
        permissions: [...s.permissions, { requestId: e.requestId, request: e.request }],
      };
      notifyCode(id, "needs your approval");
      break;
    }
    case "permissionResolved":
      next = { ...s, permissions: s.permissions.filter((p) => p.requestId !== e.requestId) };
      break;
    case "promptDone": {
      const note =
        e.stopReason === "cancelled"
          ? "Cancelled"
          : e.stopReason === "max_tokens"
            ? "Stopped: the reply hit the output token limit. Raise Max output tokens in the model's settings, or ask the agent to continue."
            : e.stopReason === "max_turn_requests"
              ? "Stopped: the agent reached its step limit for one prompt. Send Continue to go on, or raise Steps per prompt in Settings → Agents."
              : null;
      // The turn is over: a steered message was read, or went out as a prompt of its own.
      const entries = noPending(s.entries);
      next = {
        ...s,
        busy: false,
        entries: note ? [...entries, { kind: "info", text: note }] : entries,
      };
      saveTranscript(id, next);
      {
        const calls = [...Object.values(next.history?.toolCalls ?? {}), ...Object.values(next.toolCalls)];
        queueMicrotask(() => recordActivity(id, activityOf(calls)));
      }
      if (e.stopReason !== "cancelled") notifyCode(id, "finished");
      break;
    }
    case "error":
      next = { ...s, entries: [...s.entries, { kind: "error", text: e.message }] };
      break;
    case "usage":
      next = { ...s, usage: e.usage };
      break;
    case "userPrompt": {
      const attachments = e.attachments?.length ? e.attachments : undefined;
      if (!e.promptId) {
        // An older host. The window that sent it already shows it, unless this is a catch-up.
        if (e.origin === useStore.getState().mesh?.nodeId && !e.replay) return s;
        next = {
          ...s,
          busy: true,
          entries: [...s.entries, { kind: "user", text: e.text, attachments, ...(e.replay || s.status !== "ready" ? {} : { at: Date.now() }) }],
        };
        break;
      }
      // Keyed by prompt id: the sending window may already show it, and a catch-up repeats it.
      const pid = e.promptId;
      const isIt = (x: Entry) => x.kind === "user" && x.promptId === pid;
      const shown = s.entries.some(isIt);
      const queued = s.queued.filter((q) => q.promptId !== pid);
      if (e.delivery === "queued") {
        next = {
          ...s,
          entries: shown ? s.entries.filter((x) => !isIt(x)) : s.entries,
          queued: [...queued, { promptId: pid, text: e.text, attachments, origin: e.origin, replayed: e.replay }],
        };
      } else {
        next = {
          ...s,
          busy: e.delivery === "steered" ? s.busy : true,
          // Sent as a turn of its own (a steer handed back): it is no longer waiting on one.
          entries: shown
            ? s.entries.map((x) =>
                isIt(x) && x.kind === "user" && x.pending && e.delivery !== "steered" ? { ...x, pending: false } : x,
              )
            : [
                ...s.entries,
                {
                  kind: "user",
                  text: e.text,
                  attachments,
                  promptId: pid,
                  ...(e.replay || s.status !== "ready" ? {} : { at: Date.now() }),
                  ...(e.delivery === "steered" && !e.replay ? { pending: true } : {}),
                },
              ],
          queued,
        };
      }
      break;
    }
    case "unqueued": {
      const dropped = s.queued.filter((q) => e.promptIds.includes(q.promptId));
      next = {
        ...s,
        queued: s.queued.filter((q) => !e.promptIds.includes(q.promptId)),
        restore: e.restore ? handBack(s.restore, dropped, useStore.getState().mesh?.nodeId) : s.restore,
      };
      break;
    }
  }
  return next;
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
/** Same for the sidebar layout. */
let sidebarLayoutSeq = 0;

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
    flushCodeEvents(id);
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
      // A queued prompt goes out as soon as the turn ends: a restart would drop it.
      if (s.status === "ready" && !s.busy && s.permissions.length === 0 && s.queued.length === 0 && stale) {
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
  importClaudeOpen: false,
  sessionInfoId: null,
  sidebarQuery: "",
  providers: [],
  models: [],
  defaultModelRef: null,
  conversations: [],
  activeConversationId: null,
  codeSessions: [],
  sidebarLayout: null,
  activeCodeSessionId: null,
  code: {},
  chatStreams: {},
  toast: null,
  alert: null,
  meshPopoverOpen: false,
  planAccounts: [],
  planOpen: false,
  confirm: null,
  prefs: defaultPreferences,
  loadPrefs: async () => {
    const prefs = withDefaults(await api.getPreferences());
    set({ prefs });
    applyUiPrefs(prefs);
  },
  prefsSaved: (saved) => setPrefs(withDefaults(saved)),
  updatePrefs: async (patch) => {
    const next = { ...get().prefs, ...patch };
    try {
      await api.setPreferences(next);
    } catch (e) {
      return String(e);
    }
    // The backend's `prefs://changed` may have arrived first and applied them already.
    setPrefs(next);
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
  truncateCode: (id, entryIndex) => {
    flushCodeEvents(id);
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, entries: s.entries.slice(0, entryIndex), plan: null } } };
    });
  },

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
  setImportClaudeOpen: (importClaudeOpen) => set({ importClaudeOpen }),
  setSessionInfoId: (sessionInfoId) => set({ sessionInfoId }),
  setSidebarQuery: (sidebarQuery) => set({ sidebarQuery }),
  setToast: (toast) => set({ toast }),
  setAlert: (alert) => set({ alert }),
  setMeshPopoverOpen: (meshPopoverOpen) => set({ meshPopoverOpen }),
  setPlanAccounts: (planAccounts) => set({ planAccounts }),
  setPlanOpen: (planOpen) => set({ planOpen }),

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
  refreshSidebarLayout: async () => {
    const seq = ++sidebarLayoutSeq;
    const list = await api.listSidebarLayout();
    if (seq !== sidebarLayoutSeq) return;
    set({ sidebarLayout: list });
  },
  changeSidebarLayout: async (change) => {
    if (change.put.length === 0 && change.remove.length === 0) return;
    // A slower, older reload must not undo this change.
    ++sidebarLayoutSeq;
    set((st) => ({ sidebarLayout: applyChange(st.sidebarLayout ?? [], change) }));
    try {
      await api.applySidebarLayout(change.put, change.remove);
    } catch (e) {
      get().setToast(errorText(e));
      await get().refreshSidebarLayout().catch(() => {});
    }
  },
  selectCodeSession: (id) => set({ activeCodeSessionId: id }),

  resetCode: (id) => {
    flushCodeEvents(id);
    set((st) => ({ code: { ...st.code, [id]: emptyCode() } }));
  },
  pushCodeEntry: (id, e) => {
    flushCodeEvents(id);
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      const entry = (e.kind === "user" || e.kind === "agent") && e.at == null ? { ...e, at: Date.now() } : e;
      return { code: { ...st.code, [id]: { ...s, entries: [...s.entries, entry] } } };
    });
  },
  setCodeBusy: (id, busy) => {
    flushCodeEvents(id);
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: trackTurn(s, { ...s, busy }) } };
    });
  },
  clearCodeRestore: (id) =>
    set((st) => {
      const s = st.code[id];
      return s?.restore ? { code: { ...st.code, [id]: { ...s, restore: null } } } : {};
    }),

  setCodeUsage: (id, usage) =>
    set((st) => {
      const s = st.code[id] ?? emptyCode();
      return { code: { ...st.code, [id]: { ...s, usage } } };
    }),

  applyCodeEvent: (id, e) => get().applyCodeEvents(id, [e]),
  applyCodeEvents: (id, events) => {
    flushCodeEvents(id);
    const start = get().code[id];
    let s = start ?? emptyCode();
    let title: string | null = null;
    let restart = false;
    for (const e of events) {
      if (e.type === "title") title = e.title;
      else s = trackTurn(s, stepCodeEvent(id, s, e));
      if (e.type === "promptDone" || e.type === "permissionResolved") restart = true;
    }
    // One store update for the whole batch: each one re-renders the open transcript.
    set((st) => ({
      ...(s !== start ? { code: { ...st.code, [id]: s } } : {}),
      ...(title !== null
        ? { codeSessions: st.codeSessions.map((x) => (x.id === id ? { ...x, title: title!, titleManual: false } : x)) }
        : {}),
    }));
    if (restart) get().restartStaleAgents();
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
    if (has("sidebar_layout")) get().refreshSidebarLayout().catch(() => {});
    if (has("ui_prefs")) api.getPreferences().then(get().prefsSaved).catch(() => {});
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
  applyCodeResume: (id, r) => {
    flushCodeEvents(id);
    const s = get().code[id];
    if (!s) return;
    const { attach } = r;
    if (!attach.running) {
      // The agent stopped meanwhile: its exit is one of the events missed.
      get().applyCodeEvents(id, [{ type: "status", state: "exited", message: null }]);
      return;
    }
    // Calls this window still shows running: the host's status for each, else (unknown to the
    // host, so from before the agent's current run) ended once the host is idle.
    const toolCalls = { ...s.toolCalls };
    for (const [tid, tc] of Object.entries(s.toolCalls)) {
      if (tc.background || (tc.status !== "pending" && tc.status !== "in_progress")) continue;
      const status = r.tools[tid] ?? (attach.busy ? undefined : "completed");
      if (status && status !== tc.status) toolCalls[tid] = { ...tc, status: status as AcpToolCall["status"] };
    }
    // Requests answered meanwhile go; open ones the window hasn't seen arrive below.
    const open = new Set(attach.events.flatMap((e) => (e.type === "permission" ? [e.requestId] : [])));
    const permissions = s.permissions.filter((p) => open.has(p.requestId));
    const busy = attach.busy;
    set((st) => ({
      code: {
        ...st.code,
        [id]: trackTurn(s, { ...s, toolCalls, permissions, busy, entries: busy ? s.entries : noPending(s.entries) }),
      },
    }));
    const shown = new Set(permissions.map((p) => p.requestId));
    const replay = attach.events.filter(
      (e) => (e.type === "permission" && !shown.has(e.requestId)) || e.type === "usage" || e.type === "update",
    );
    if (replay.length) get().applyCodeEvents(id, replay);
  },
  attachAgent: async (id) => {
    flushCodeEvents(id);
    // Live events that arrive while the catch-up is in flight wait, so none is lost or doubled.
    attaching.set(id, []);
    lastPoint.delete(id);
    const finish = (point?: EventPoint | null) => {
      const held = attaching.get(id) ?? [];
      attaching.delete(id);
      const fresh = held.filter(({ pt }) => !(point && pt && pt.s === point.s && pt.n <= point.n));
      if (fresh.length) get().applyCodeEvents(id, fresh.map(({ e }) => e));
      const last = fresh.findLast((x) => x.pt)?.pt ?? point;
      if (last) lastPoint.set(id, last);
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
      code: { ...st.code, [id]: { ...emptyCode(), configRev: rev, catalogRev, history, earlier, status: "ready", busy: info.busy, turnStartedAt: info.busy ? Date.now() : null } },
    }));
    get().applyCodeEvents(id, info.events);
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

/** The point of the last live event applied, per session. */
const lastPoint = new Map<string, EventPoint>();
/** When each session last asked its host to catch up, so a dropped link doesn't flood it. */
const resyncedAt = new Map<string, number>();
const RESYNC_GAP_MS = 5_000;

const isRunning = (tc: AcpToolCall) => !tc.background && (tc.status === "pending" || tc.status === "in_progress");

/**
 * Catch session `id` up with events this window missed: the host sends the ones after `from`
 * (by default the last applied) or, when it no longer has them, a snapshot of the turn. Live
 * events wait meanwhile, as for an attach. Desktop and browser windows only: the phone app
 * resumes through its gateway (`remote://resync`).
 */
export async function resyncCode(id: string, from?: EventPoint) {
  if (isRemoteApp() || attaching.has(id) || !useStore.getState().code[id]) return;
  const now = Date.now();
  if (now - (resyncedAt.get(id) ?? 0) < RESYNC_GAP_MS) return;
  resyncedAt.set(id, now);
  const start = from ?? lastPoint.get(id) ?? null;
  flushCodeEvents(id);
  attaching.set(id, []);
  const running = Object.values(useStore.getState().code[id]?.toolCalls ?? {})
    .filter(isRunning)
    .map((tc) => tc.toolCallId);
  let r: CodeResume | null = null;
  try {
    r = await api.codeResume(id, start, running);
  } catch {
    // An older host, or the request failed: try again on the next trigger.
    resyncedAt.delete(id);
  }
  const held = attaching.get(id) ?? [];
  attaching.delete(id);
  const st = useStore.getState();
  let point: EventPoint | null = null;
  if (r?.mode === "delta") {
    const after = r.events.filter((e) => !(start && e._pt && e._pt.s === start.s && e._pt.n <= start.n));
    if (after.length) st.applyCodeEvents(id, after.map((e) => e.event));
    point = r.point;
  } else if (r?.mode === "snapshot") {
    st.applyCodeResume(id, r);
    point = r.point;
  } else if (start) {
    // Nothing caught up: resume from before the gap next time.
    lastPoint.set(id, start);
  }
  const fresh = held.filter(({ pt }) => !(point && pt && pt.s === point.s && pt.n <= point.n));
  if (fresh.length) st.applyCodeEvents(id, fresh.map(({ e }) => e));
  const last = fresh.findLast((x) => x.pt)?.pt ?? point;
  if (last) lastPoint.set(id, last);
}

/**
 * Catch up the sessions this window shows that run on other computers: after the mesh
 * reconnects (events sent while it was down are gone), and now and then during a turn.
 * `busyOnly` limits it to sessions mid-turn.
 */
export function resyncRemoteCode(busyOnly: boolean) {
  const st = useStore.getState();
  for (const [id, s] of Object.entries(st.code)) {
    if (s.status === "idle" || (busyOnly && !s.busy)) continue;
    const sess = st.codeSessions.find((c) => c.id === id);
    if (remoteHost(sess, st.mesh)) void resyncCode(id);
  }
}

/** Live events not applied yet, per session: they go in together on the next flush. */
const pendingEvents = new Map<string, CodeEvent[]>();
let flushTimer: ReturnType<typeof setTimeout> | null = null;
/**
 * How long live events gather before they are applied. An agent replaying a long session sends
 * thousands of events, each in its own task; one store update (and render) per event made opening
 * such a session take minutes.
 */
const EVENT_BATCH_MS = 16;

/** A live Code event from the backend: applied shortly, or after the catch-up in progress. */
export function codeEventArrived(env: CodeEnvelope) {
  const id = env.codeSessionId;
  const pt = env._pt;
  // Points number each session's events (see docs/design/mobile.md, section 3). The phone app
  // resumes from them through its gateway; here they show events this window missed, such as
  // those sent while the connection to the session's host was down.
  if (pt && !isRemoteApp() && !attaching.has(id)) {
    const last = lastPoint.get(id);
    // Already applied: a catch-up brought it.
    if (last && last.s === pt.s && pt.n <= last.n) return;
    lastPoint.set(id, pt);
    const missed = last ? (last.s === pt.s ? pt.n > last.n + 1 : pt.n > 1) : false;
    if (missed) void resyncCode(id, last);
  }
  const held = attaching.get(id);
  if (held) {
    held.push({ e: env.event, pt });
    return;
  }
  const queue = pendingEvents.get(id);
  if (queue) queue.push(env.event);
  else pendingEvents.set(id, [env.event]);
  // A hidden window may run timers late or not at all, and its notifications must not wait.
  if (document.hidden) flushCodeEvents(id);
  else flushTimer ??= setTimeout(() => flushCodeEvents(), EVENT_BATCH_MS);
}

/**
 * Applies the live events still waiting, for session `id` or for all. Anything that changes a
 * session's state directly flushes first, so it lands after the events that arrived before it.
 */
export function flushCodeEvents(id?: string) {
  const ids = id === undefined ? [...pendingEvents.keys()] : pendingEvents.has(id) ? [id] : [];
  for (const sid of ids) {
    const events = pendingEvents.get(sid)!;
    pendingEvents.delete(sid);
    useStore.getState().applyCodeEvents(sid, events);
  }
  if (pendingEvents.size === 0 && flushTimer !== null) {
    clearTimeout(flushTimer);
    flushTimer = null;
  }
}

function notifyCode(id: string, what: string) {
  const title = useStore.getState().codeSessions.find((c) => c.id === id)?.title ?? "Code session";
  notifyInBackground(`Agent ${what}`, title, { kind: "code", id });
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
  // With Enter to send, Mod+Enter sends too (and force-sends mid-turn); Shift+Enter breaks the line.
  return sendKey === "mod-enter" ? e.ctrlKey || e.metaKey : !e.shiftKey;
}

/**
 * True when this keydown should send now, cancelling the agent's running turn. Mod+Enter, or
 * Mod+Shift+Enter where Mod+Enter is already the send key. Check it before {@link isSendKey}.
 */
export function isForceSendKey(e: { key: string; shiftKey: boolean; ctrlKey: boolean; metaKey: boolean }, sendKey: Preferences["sendKey"]) {
  if (e.key !== "Enter" || !(e.ctrlKey || e.metaKey)) return false;
  return modEnterSends(sendKey) ? e.shiftKey : true;
}

/** How to say the force-send key in a tooltip. */
export function forceSendKeyLabel(sendKey: Preferences["sendKey"]) {
  return modEnterSends(sendKey) ? "Ctrl+Shift+Enter" : "Ctrl+Enter";
}

function modEnterSends(sendKey: Preferences["sendKey"]) {
  return isRemoteApp() || sendKey === "mod-enter";
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
