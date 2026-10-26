import { Channel, invoke } from "@tauri-apps/api/core";
import type { Activity } from "./activity";
import type { UpdateInfo } from "./update";

export type ProviderKind = "openai_chat" | "openai_responses" | "anthropic" | "gemini";
export type AuthMode = "api_key" | "claude_login";

export interface Provider {
  id: string;
  name: string;
  kind: ProviderKind;
  baseUrl: string;
  hasKey: boolean;
  authMode: AuthMode;
}

/** USD per 1M tokens. */
export interface Pricing {
  input: number | null;
  cachedInput: number | null;
  cacheWrite: number | null;
  output: number | null;
}

export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
export type ReasoningSummary = "none" | "auto" | "concise" | "detailed";

export interface ModelParams {
  maxOutputTokens: number | null;
  temperature: number | null;
  topP: number | null;
  reasoningEffort: ReasoningEffort | null;
  reasoningSummary: ReasoningSummary | null;
  autoCompactPercent: number | null;
  pricing: Pricing;
}

export interface Model {
  id: string;
  providerId: string;
  modelId: string;
  displayName: string;
  vision: boolean;
  contextWindow: number | null;
  params: ModelParams;
}

export const emptyParams = (): ModelParams => ({
  maxOutputTokens: null,
  temperature: null,
  topP: null,
  reasoningEffort: null,
  reasoningSummary: null,
  autoCompactPercent: null,
  pricing: { input: null, cachedInput: null, cacheWrite: null, output: null },
});

/** Cost from user-entered prices; null when no price applies to any bucket. */
export function estimateCost(t: Tokens, p: Pricing | undefined | null): number | null {
  if (!p) return null;
  const fresh = Math.max(0, t.input - t.cachedRead - t.cacheWrite);
  const parts: [number, number | null][] = [
    [fresh, p.input],
    // Unpriced cache buckets fall back to the regular input price.
    [t.cachedRead, p.cachedInput ?? p.input],
    [t.cacheWrite, p.cacheWrite ?? p.input],
    [t.output, p.output],
  ];
  if (!parts.some(([n, price]) => n > 0 && price != null)) return null;
  return parts.reduce((sum, [n, price]) => sum + (price != null ? (n * price) / 1_000_000 : 0), 0);
}

export interface DiscoveredModel {
  id: string;
  displayName: string;
  contextWindow: number | null;
}

/** Token counts; `input` includes the cached buckets. */
export interface Tokens {
  input: number;
  cachedRead: number;
  cacheWrite: number;
  output: number;
  reasoning: number;
  costUsd: number | null;
  apiMs: number;
}

export interface UsageView {
  totals: Tokens;
  contextTokens: number | null;
  contextWindow: number | null;
  autoCompactPercent: number | null;
}

/** One Claude plan usage window: the fraction used (0 to 1) and when it resets (unix seconds). */
export interface PlanWindow {
  utilization: number;
  resetsAt: number;
}

/** The Claude plan usage Claude Code last reported (see `code/plan_limits.rs`). */
export interface PlanLimits {
  fiveHour: PlanWindow | null;
  sevenDay: PlanWindow | null;
  /** Billing extra usage, past the plan, until this time (unix seconds). */
  overageUntil: number | null;
  observedAt: number;
}

/** One kept report of the plan windows, for the usage panel's history of busy periods. */
export interface PlanSample {
  at: number;
  fiveHour: PlanWindow | null;
  sevenDay: PlanWindow | null;
}

/** The ACP wire log's state (see `code/wire.rs`). */
export interface WireLogStatus {
  on: boolean;
  /** Until it turns itself off. */
  remainingSecs: number;
  frames: number;
  /** Frames dropped to stay under the size cap. */
  dropped: number;
}

/** One raw ACP frame, redacted and cut to size. `dir`: from the agent (`in`) or to it (`out`). */
export interface WireFrame {
  seq: number;
  at: number;
  session: string;
  agent: string;
  dir: "in" | "out";
  text: string;
}

export interface Conversation {
  id: string;
  title: string;
  modelRef: string | null;
  systemPrompt: string | null;
  pinned: boolean;
  createdAt: number;
  updatedAt: number;
  /** When it was archived (unix ms): out of the lists until restored. */
  archivedAt?: number | null;
}

export interface Attachment {
  id: string;
  sha256: string;
  name: string;
  mime: string;
  kind: "image" | "text" | "pdf";
  size: number;
  path: string;
}

export interface Message {
  id: string;
  conversationId: string;
  role: "user" | "assistant";
  content: string;
  reasoning: string | null;
  modelRef: string | null;
  error: string | null;
  createdAt: number;
  attachments: Attachment[];
  usage: Tokens | null;
  parentId: string | null;
  /** This message and its alternatives (same parent and role), oldest first. */
  siblingIds: string[];
}

export interface SearchHit {
  conversationId: string;
  conversationTitle: string;
  messageId: string;
  role: "user" | "assistant";
  /** Matches are wrapped in \u0001…\u0002. */
  snippet: string;
  createdAt: number;
}

export interface KeyValue {
  name: string;
  value: string;
  secret: boolean;
  /** Output only: a secret value is stored. */
  stored?: boolean;
}

export interface McpConfig {
  transport: "stdio" | "http" | "sse";
  command: string;
  args: string[];
  env: KeyValue[];
  url: string;
  headers: KeyValue[];
}

export interface McpServer {
  id: string;
  name: string;
  enabled: boolean;
  config: McpConfig;
}

export interface PermissionRule {
  id: string;
  /** `*` = every folder. */
  cwd: string;
  toolKind: string;
  pattern: string;
  createdAt: number;
}

export interface Prompt {
  id: string;
  name: string;
  content: string;
  createdAt: number;
  updatedAt: number;
}

export interface UsageReport {
  chat: { day: string; modelRef: string | null; replies: number; usage: Tokens }[];
  code: { id: string; title: string; cwd: string; modelRef: string | null; agent: AgentId; agentModel: string | null; updatedAt: number; usage: Tokens }[];
}

/** `agentCapabilities` from ACP `initialize` (subset we read). */
export interface AgentCapabilities {
  loadSession?: boolean;
  promptCapabilities?: { image?: boolean; audio?: boolean; embeddedContext?: boolean };
  mcpCapabilities?: { http?: boolean; sse?: boolean };
}

export interface CodeSession {
  id: string;
  cwd: string;
  acpSessionId: string | null;
  title: string;
  modelRef: string | null;
  pinned: boolean;
  createdAt: number;
  updatedAt: number;
  /** What the session has done, for its sidebar icon; null until its transcript was checked. */
  activity: Activity | null;
  /** Coding agent that runs the session. */
  agent: AgentId;
  /** The agent's own model id, for agents that keep their own model list. */
  agentModel: string | null;
  /** Mesh node running this session's agent. */
  hostNode: string | null;
  /** Bumped on every move between nodes. */
  epoch: number;
  /** Ask / auto / always-approve: Patchbay answers ACP permission prompts for this session. */
  permissionMode: "default" | "auto" | "always-approve";
  /** When it was archived (unix ms): its agent stopped, out of the lists until restored. */
  archivedAt?: number | null;
  /** Someone named it: automatic titles leave it alone until "Auto-retitle". */
  titleManual?: boolean;
}

// ---- mesh ----

export type MeshMemberState = "self" | "direct" | "relay" | "offline" | "removed";

export interface MeshMember {
  nodeId: string;
  name: string;
  state: MeshMemberState;
  /** For "relay": the member traffic goes through. */
  via: string | null;
  viaName: string | null;
  rttMs: number | null;
  url: string | null;
  admittedAt: number;
  approver: string;
  /** "remote" for a phone that only drives other members' work. */
  role?: string | null;
  /** Its handshake with this computer failed on the passphrase. */
  authFailed?: boolean;
}

/** Something wrong with this computer's mesh that the user has to fix. */
export interface MeshProblem {
  kind: "listen" | "passphrase" | "auth";
  title: string;
  message: string;
}

export interface MeshStatus {
  nodeId: string;
  /** On a phone: its connection to the gateway. */
  phase?: "offline" | "connecting" | "online";
  name: string;
  member: boolean;
  meshId: string | null;
  running: boolean;
  listen: string;
  port: number | null;
  advertise: string[];
  joining: {
    url: string;
    code: string | null;
    status: "connecting" | "waiting" | "failed" | "rejected";
    error: string | null;
  } | null;
  members: MeshMember[];
  /** Nodes waiting to join. Any member decides them, not only `host`, the one they are connected to. */
  pending: { nodeId: string; name: string; code: string; role?: string | null; host?: string | null; hostName?: string | null }[];
  problem?: MeshProblem | null;
}

/** A chat reply streaming on another node. */
export interface RemoteStream {
  conversationId: string;
  streamer: string;
  streamerName: string;
  userMessageId: string | null;
  assistantMessageId: string | null;
  text: string;
  reasoning: string;
  status: string | null;
}

export interface MeshChatEvent {
  conversationId: string;
  streamer: string;
  streamerName: string;
  event:
    | ChatEvent
    | { type: "snapshot"; userMessageId: string | null; assistantMessageId: string | null; text: string; reasoning: string };
}

export interface CodeAttach {
  running: boolean;
  busy: boolean;
  events: CodeEvent[];
  /** The host isn't connected. */
  offline?: boolean;
  /** The last live event the catch-up covers (phone app); later ones continue from here. */
  point?: EventPoint | null;
}

export type AgentId = "patchbay" | "grok" | "claude-code" | "opencode" | "cursor" | "kilo" | "codex" | "antigravity";

/** One coding agent backend, as the Agents settings page lists it. */
export interface AgentInfo {
  id: AgentId;
  label: string;
  available: boolean;
  program: string | null;
  source: "builtin" | "override" | "path" | "npx" | null;
  version: string | null;
  error: string | null;
  installHint: string;
  usesPatchbayModels: boolean;
  supportsInstructions: boolean;
  /** Can be set to run on Patchbay's providers instead of its own sign-in. */
  takesProviders: boolean;
}

/** Labels for agent ids, for places that don't load the full list. */
export const AGENT_LABELS: Record<AgentId, string> = {
  patchbay: "Patchbay",
  grok: "Grok Build",
  "claude-code": "Claude Code",
  opencode: "OpenCode",
  cursor: "Cursor CLI",
  kilo: "Kilo Code",
  codex: "Codex",
  antigravity: "Antigravity",
};

/** Agents that run on Patchbay's own providers and models (`pb-<id>` model ids). */
export const usesPatchbayModels = (agent: string): boolean => agent === "patchbay" || agent === "grok";

export type PromptDelivery = "sent" | "steered" | "queued";

/** Non-standard ACP extensions the session's agent understands. */
export interface AgentExtensions {
  statusLine: boolean;
  rewind: boolean;
  /** Runs on Patchbay's providers and models: a change to them needs a restart to apply. */
  patchbayModels?: boolean;
  /** A message sent mid-turn joins the running turn; otherwise it waits for the turn to end. */
  steering?: boolean;
  /** The host can cancel the running turn to send a message next (older hosts leave it out). */
  forceSend?: boolean;
}

/** An ACP session config option (only `select` ones are shown). */
export interface AcpConfigOption {
  id: string;
  name: string;
  description?: string | null;
  category?: string | null;
  type: string;
  currentValue: string;
  options: AcpConfigChoice[];
}

export type AcpConfigChoice =
  | { value: string; name: string; description?: string | null }
  | { group: string; name: string; options: { value: string; name: string; description?: string | null }[] };

export type ChatEvent =
  | { type: "started"; userMessageId: string; assistantMessageId: string }
  | { type: "text"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "status"; message: string }
  | { type: "done"; stopReason: string | null; usage: Tokens | null }
  | { type: "error"; message: string }
  | { type: "title"; title: string };

/** A sign-in method the agent advertised, as Patchbay can run it (`acp::AuthChoice`). */
export interface AuthChoice {
  id: string;
  name: string;
  description: string | null;
  /** The method takes an API key the user types in. */
  apiKey: boolean;
  /** The user signs in in a terminal running the agent's login command. */
  terminal: boolean;
}

export type CodeEvent =
  | { type: "status"; state: "starting" | "ready" | "exited"; message: string | null }
  | {
      type: "sessionReady";
      acpSessionId: string;
      models: AcpModels | null;
      modes: AcpModes | null;
      loaded: boolean;
      capabilities: AgentCapabilities | null;
      agentInfo: { name?: string; title?: string; version?: string } | null;
      configOptions: AcpConfigOption[] | null;
      extensions: AgentExtensions;
      /** What this agent does without asking, whatever the session's approval mode. */
      approvalGaps?: string[];
    }
  | { type: "config"; models: AcpModels | null; modes: AcpModes | null; configOptions: AcpConfigOption[] | null }
  | { type: "autoApproved"; toolCallId: string; description: string }
  | { type: "notice"; message: string }
  | { type: "update"; update: AcpUpdate }
  | { type: "permission"; requestId: string; request: AcpPermissionRequest }
  | { type: "permissionResolved"; requestId: string }
  | { type: "promptDone"; stopReason: string }
  | { type: "error"; message: string }
  | { type: "usage"; usage: UsageView }
  /**
   * A prompt sent to the agent from `origin` (a mesh node). `delivery`: a new turn, steered into
   * the running turn, or queued behind it (it comes again as `sent` when its turn starts).
   * `replay`: part of a catch-up. Older hosts send neither `promptId` nor `delivery`.
   */
  | {
      type: "userPrompt";
      promptId?: string;
      text: string;
      origin: string;
      attachments: Attachment[];
      delivery?: PromptDelivery;
      replay?: boolean;
    }
  /** Queued prompts that won't be sent. `restore`: put them back in the sender's composer. */
  | { type: "unqueued"; promptIds: string[]; restore: boolean }
  /** The session was renamed automatically. */
  | { type: "title"; title: string }
  /** The agent wants a sign-in with one of `methods`: before its session starts, or mid-session. */
  | { type: "authRequired"; message: string; methods: AuthChoice[] }
  /** A mid-session sign-in worked; the session goes on. */
  | { type: "authResolved" }
  /** Something the agent sent that Patchbay can't show; `raw` is its payload. */
  | { type: "unrecognized"; message: string; raw: unknown };

/** Where a live event sits in its topic's stream (see docs/design/mobile.md, section 3). */
export interface EventPoint {
  s: string;
  n: number;
}

export interface CodeEnvelope {
  codeSessionId: string;
  event: CodeEvent;
  _pt?: EventPoint;
}

// ---- ACP wire shapes (subset) ----

export interface AcpModels {
  availableModels: { modelId: string; name: string; description?: string | null }[];
  currentModelId: string;
}

export interface AcpModes {
  availableModes: { id: string; name: string; description?: string | null }[];
  currentModeId: string;
}

export type AcpContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mimeType: string; uri?: string | null }
  | { type: "resource_link"; name: string; uri: string; mimeType?: string | null }
  | { type: "resource"; resource: { uri: string; text?: string; mimeType?: string | null } };

export type AcpToolCallContent =
  | { type: "content"; content: AcpContentBlock }
  | { type: "diff"; path: string; oldText: string | null; newText: string }
  | { type: "terminal"; terminalId: string };

export interface ClaudeSession {
  id: string;
  cwd: string;
  cwdExists: boolean;
  title: string;
  preview: string;
  /** Unix ms. */
  modified: number;
  size: number;
  /** The Code session an earlier import made. */
  imported: string | null;
}

export interface AcpToolCall {
  toolCallId: string;
  title?: string;
  kind?: string;
  status?: "pending" | "in_progress" | "completed" | "failed";
  content?: AcpToolCallContent[] | null;
  locations?: { path: string; line?: number | null }[] | null;
  rawInput?: unknown;
  rawOutput?: unknown;
  /** Patchbay's own, from `_meta.terminal_*`: the command's output so far. */
  terminalOutput?: string;
  /** Patchbay's own: the command has a terminal (`_meta.terminal_info`). */
  terminal?: boolean;
  /** Patchbay's own: the command ended (`_meta.terminal_exit`); `exitCode` null when unknown. */
  terminalExit?: { exitCode: number | null };
  /**
   * Patchbay's own: the tool call finished but its command still runs, backgrounded by the agent
   * (a terminal with no exit yet). Cleared by its `terminal_exit` or when the agent stops.
   */
  background?: boolean;
}

export interface AcpCommand {
  name: string;
  description: string;
  input?: { hint: string } | null;
}

export interface AcpPlanEntry {
  content: string;
  priority: string;
  /** `cancelled` is from ACP's v2 draft; OpenCode's to-do list uses it. */
  status: "pending" | "in_progress" | "completed" | "cancelled";
}

export type AcpUpdate =
  | { sessionUpdate: "user_message_chunk"; content: AcpContentBlock }
  | { sessionUpdate: "agent_message_chunk"; content: AcpContentBlock }
  | { sessionUpdate: "agent_thought_chunk"; content: AcpContentBlock }
  | ({ sessionUpdate: "tool_call" } & AcpToolCall)
  | ({ sessionUpdate: "tool_call_update" } & AcpToolCall)
  | { sessionUpdate: "plan"; entries: AcpPlanEntry[] }
  | { sessionUpdate: "current_mode_update"; currentModeId: string }
  | { sessionUpdate: "available_commands_update"; availableCommands: AcpCommand[] }
  | { sessionUpdate: string; [k: string]: unknown };

export interface AcpPermissionOption {
  optionId: string;
  name: string;
  kind: "allow_once" | "allow_always" | "reject_once" | "reject_always";
}

export interface AcpPermissionRequest {
  sessionId: string;
  toolCall: AcpToolCall;
  options: AcpPermissionOption[];
}

/** What the Code agent loads from your Claude Code setup (`~/.claude`, project `.claude/`). */
export interface ClaudeConfig {
  skills: boolean;
  instructions: boolean;
  agents: boolean;
  mcpServers: boolean;
  hooks: boolean;
  plugins: boolean;
  /** Plugins (`name@marketplace`) Patchbay's agent doesn't load. */
  disabledPlugins: string[];
  /** MCP servers from Claude's config Patchbay's agent doesn't start. */
  disabledMcpServers: string[];
}

/** One part of the Claude Code setup and where it comes from (project, user, plugin <name>). */
export interface ClaudeItem {
  name: string;
  description: string;
  source: string;
}

/** What Patchbay's agent finds in the user's Claude Code setup, whatever the switches say. */
export interface ClaudeOverview {
  home: string | null;
  project: string | null;
  instructions: string[];
  plugins: { id: string; name: string; enabledInClaude: boolean; disabled: boolean }[];
  commands: ClaudeItem[];
  skills: ClaudeItem[];
  agents: ClaudeItem[];
  mcpServers: { name: string; source: string; transport: string; disabled: boolean }[];
  hooks: { event: string; matcher: string; command: string; source: string }[];
}

/** Limits of Patchbay's own agent; `null` keeps its default. */
export interface AgentEngine {
  /** Model requests one prompt may make (default 100). */
  maxSteps: number | null;
  /** Model requests one subagent run may make (default 40). */
  subagentMaxSteps: number | null;
  /** Retries of a request that fails before any output (default 2). */
  requestRetries: number | null;
  /** Seconds a reply stream may stay silent (default 180). */
  idleTimeoutSecs: number | null;
  /** Default shell command timeout in seconds (default 120). */
  shellTimeoutSecs: number | null;
}

export interface Preferences {
  theme: "system" | "light" | "dark";
  textSize: "small" | "default" | "large";
  sendKey: "enter" | "mod-enter";
  launchTab: "chat" | "code" | "last";
  notifications: boolean;
  quickEntryShortcut: string;
  /** system: the window manager's title bar; app: none, Patchbay draws the window controls. */
  titleBar: "system" | "app";
  defaultSystemPrompt: string;
  expandReasoning: boolean;
  generateTitles: boolean;
  /** Let the model search the web in chat: the provider's own search, else Brave Search when a key is set. */
  chatWebSearch: boolean;
  defaultAgent: AgentId;
  /** Executable overrides: agent id -> path. */
  agentPaths: Partial<Record<AgentId, string>>;
  /** Agents set to run on Patchbay's providers: agent id -> Patchbay model ref. */
  agentProviders: Partial<Record<AgentId, string>>;
  /** Stop turns on a Claude plan (Claude Code, or Patchbay's agent on a Claude login) at `percent`. */
  usageLimit: { enabled: boolean; percent: number };
  permissionMode: "default" | "auto" | "always-approve";
  agentInstructions: string;
  webFetch: boolean;
  minifyPrompts: boolean;
  autoCompactPercent: number | null;
  backgroundSummaries: boolean;
  claudeConfig: ClaudeConfig;
  agentEngine: AgentEngine;
  terminalShell: string;
  terminalFontSize: number;
  /** Draw typed characters before a slow shell echoes them: auto = when echo lags. */
  terminalLocalEcho: "auto" | "on" | "off";
  /** auto | system | builtin | server. This device only; preferences are not synced. */
  readAloudEngine: "auto" | "system" | "builtin" | "server";
  /** Voice id for the engine in use. Empty means that engine's default. */
  readAloudVoice: string;
  readAloudRate: "slower" | "normal" | "faster";
  /** Speak each new finished reply while its chat or session is on screen. */
  readAloudListen: boolean;
  /** OpenAI-compatible speech base URL, used when no provider is chosen. */
  readAloudServerUrl: string;
  readAloudServerModel: string;
  readAloudServerVoice: string;
  /** An OpenAI-compatible provider whose key and URL to use. */
  readAloudServerProviderId: string;
  /** Model ref that rewrites a reply for the ear before it is spoken. Empty means off. */
  readAloudRewriteModel: string;
}

export const defaultPreferences: Preferences = {
  theme: "system",
  textSize: "default",
  sendKey: "enter",
  launchTab: "last",
  notifications: true,
  quickEntryShortcut: "",
  titleBar: "system",
  defaultSystemPrompt: "",
  expandReasoning: false,
  generateTitles: true,
  chatWebSearch: false,
  defaultAgent: "patchbay",
  agentPaths: {},
  agentProviders: {},
  usageLimit: { enabled: true, percent: 95 },
  permissionMode: "default",
  agentInstructions: "",
  webFetch: false,
  minifyPrompts: false,
  autoCompactPercent: null,
  backgroundSummaries: true,
  claudeConfig: {
    skills: true,
    instructions: true,
    agents: true,
    mcpServers: true,
    hooks: true,
    plugins: true,
    disabledPlugins: [],
    disabledMcpServers: [],
  },
  agentEngine: { maxSteps: null, subagentMaxSteps: null, requestRetries: null, idleTimeoutSecs: null, shellTimeoutSecs: null },
  terminalShell: "",
  terminalFontSize: 13,
  terminalLocalEcho: "auto",
  readAloudEngine: "auto",
  readAloudVoice: "",
  readAloudRate: "normal",
  readAloudListen: false,
  readAloudServerUrl: "",
  readAloudServerModel: "",
  readAloudServerVoice: "",
  readAloudServerProviderId: "",
  readAloudRewriteModel: "",
};

export interface SpeechAudio {
  mime: string;
  audioBase64: string;
}

export interface NativeVoice {
  id: string;
  name: string;
}

export type TermEvent = { type: "data"; data: string } | { type: "exit"; code: number | null };

// ---- commands ----

/** One folder listing (gateway.rs, `list_dirs_local`). */
export interface DirListing {
  path: string;
  parent: string | null;
  home: string;
  dirs: string[];
  truncated: boolean;
}

/** Which Patchbay build a computer runs (gateway.rs, `host_info_local`). */
export interface HostInfo {
  version: string;
  /** Rust's `std::env::consts::OS`: "macos", "linux", "windows". */
  os: string;
  /** Rust's `std::env::consts::ARCH`: "aarch64", "x86_64". */
  arch: string;
}

/** What a computer offers for a new Code session (gateway.rs, `new_session_options_local`). */
export interface NewSessionOptions {
  agents: AgentInfo[];
  defaultAgent: AgentId;
  home: string | null;
  /** Folders of its recent sessions, newest first. */
  recent: string[];
}

export const api = {
  listProviders: () => invoke<Provider[]>("list_providers"),
  saveProvider: (input: {
    id?: string;
    name: string;
    kind: ProviderKind;
    baseUrl: string;
    apiKey?: string;
    authMode?: AuthMode;
  }) => invoke<Provider>("save_provider", { input }),
  deleteProvider: (id: string) => invoke<void>("delete_provider", { id }),
  discoverModels: (providerId: string) => invoke<DiscoveredModel[]>("discover_models", { providerId }),
  listModels: () => invoke<Model[]>("list_models"),
  saveModel: (input: {
    id?: string;
    providerId: string;
    modelId: string;
    displayName?: string;
    vision?: boolean;
    contextWindow?: number | null;
    params?: ModelParams;
  }) => invoke<Model>("save_model", { input }),
  deleteModel: (id: string) => invoke<void>("delete_model", { id }),
  getPreferences: () => invoke<Preferences>("get_preferences"),
  setPreferences: (prefs: Preferences) => invoke<void>("set_preferences", { prefs }),
  webSearchStatus: () =>
    invoke<{ configured: boolean; source: "env" | "keychain" | "database" | null }>("web_search_status"),
  claudeOverview: (cwd: string | null) => invoke<ClaudeOverview>("claude_overview", { cwd }),
  setWebSearchKey: (key: string) => invoke<void>("set_web_search_key", { key }),
  clearWebSearchKey: () => invoke<void>("clear_web_search_key"),
  listNativeVoices: () => invoke<NativeVoice[]>("list_native_voices"),
  speakNative: (text: string, voice: string, rate: string) => invoke<void>("speak_native", { text, voice, rate }),
  stopNative: () => invoke<void>("stop_native"),
  synthesizeSpeech: (input: {
    providerId: string;
    baseUrl: string;
    model: string;
    voice: string;
    text: string;
  }) => invoke<SpeechAudio>("synthesize_speech", input),
  rewriteForSpeech: (modelRef: string, text: string) => invoke<string>("rewrite_for_speech", { modelRef, text }),
  getSetting: (key: string) => invoke<string | null>("get_setting", { key }),
  setSetting: (key: string, value: string) => invoke<void>("set_setting", { key, value }),

  listConversations: () => invoke<Conversation[]>("list_conversations"),
  createConversation: (modelRef: string | null) => invoke<Conversation>("create_conversation", { modelRef }),
  updateConversation: (
    id: string,
    patch: { title?: string; modelRef?: string; systemPrompt?: string; pinned?: boolean; archived?: boolean },
  ) =>
    invoke<void>("update_conversation", { id, ...patch }),
  deleteConversation: (id: string) => invoke<void>("delete_conversation", { id }),
  listMessages: (conversationId: string) => invoke<Message[]>("list_messages", { conversationId }),
  switchBranch: (conversationId: string, messageId: string) =>
    invoke<void>("switch_branch", { conversationId, messageId }),
  searchMessages: (query: string, limit?: number) => invoke<SearchHit[]>("search_messages", { query, limit }),
  /** `parentId`: omitted = continue the visible branch, `""` = new first message, id = branch under it. */
  sendChatMessage: (
    conversationId: string,
    modelRef: string,
    text: string,
    attachmentIds: string[],
    opts: { parentId?: string; reasoningEffort?: string | null },
    onEvent: (e: ChatEvent) => void,
  ) => {
    const ch = new Channel<ChatEvent>();
    ch.onmessage = onEvent;
    return invoke<void>("send_chat_message", {
      conversationId,
      modelRef,
      text,
      attachmentIds,
      parentId: opts.parentId,
      reasoningEffort: opts.reasoningEffort ?? undefined,
      onEvent: ch,
    });
  },
  regenerateReply: (
    conversationId: string,
    modelRef: string,
    userMessageId: string,
    reasoningEffort: string | null,
    onEvent: (e: ChatEvent) => void,
  ) => {
    const ch = new Channel<ChatEvent>();
    ch.onmessage = onEvent;
    return invoke<void>("regenerate_reply", {
      conversationId,
      modelRef,
      userMessageId,
      reasoningEffort: reasoningEffort ?? undefined,
      onEvent: ch,
    });
  },
  cancelChat: (conversationId: string) => invoke<void>("cancel_chat", { conversationId }),

  addAttachmentPath: (path: string) => invoke<Attachment>("add_attachment_path", { path }),
  addAttachmentBytes: (name: string, dataBase64: string) =>
    invoke<Attachment>("add_attachment_bytes", { name, dataBase64 }),
  attachmentThumbnail: (id: string) => invoke<string>("attachment_thumbnail", { id }),
  attachmentImage: (id: string) => invoke<string>("attachment_image", { id }),

  listCodeSessions: () => invoke<CodeSession[]>("list_code_sessions"),
  /** With `host`, the session runs on that computer, in its folder `cwd`. */
  createCodeSession: (cwd: string, modelRef: string | null, agent?: AgentId, host?: string | null) =>
    invoke<CodeSession>("create_code_session", { cwd, modelRef, agent: agent ?? null, host: host ?? null }),
  /** Claude Code's sessions on this computer. */
  listClaudeSessions: () => invoke<ClaudeSession[]>("list_claude_sessions"),
  /** Make Code sessions from Claude Code's, continued by `agent` (default: the preferred one). */
  importClaudeSessions: (ids: string[], agent?: AgentId | null) =>
    invoke<CodeSession[]>("import_claude_sessions", { ids, agent: agent ?? null }),
  /** With `host`, the agents installed on that computer. */
  listAgents: (host?: string | null) => invoke<AgentInfo[]>("list_agents", { host: host ?? null }),
  updateCodeSession: (
    id: string,
    patch: { title?: string; modelRef?: string; pinned?: boolean; activity?: Activity; archived?: boolean },
  ) =>
    invoke<void>("update_code_session", { id, ...patch }),
  deleteCodeSession: (id: string) => invoke<void>("delete_code_session", { id }),
  /** Name the session from its conversation, replacing a title someone typed. Returns the title. */
  codeAutoTitle: (id: string) => invoke<string>("code_auto_title", { id }),
  codeStart: (id: string) => invoke<void>("code_start", { id }),
  codeIsRunning: (id: string) => invoke<boolean>("code_is_running", { id }),
  codeStop: (id: string) => invoke<void>("code_stop", { id }),
  /**
   * `mentions`: absolute paths of @-mentioned project files. `promptId` names the prompt in the
   * `userPrompt` events it causes. `force` cancels the running turn and sends this next.
   */
  codePrompt: (id: string, text: string, attachmentIds: string[], mentions: string[] = [], promptId?: string, force?: boolean) =>
    invoke<void>("code_prompt", { id, text, attachmentIds, mentions, promptId, force }),
  /** Drop a prompt queued behind the running turn. */
  codeUnqueue: (id: string, promptId: string) => invoke<void>("code_unqueue", { id, promptId }),
  /** Cancel the running turn and send this queued prompt next. */
  codeSendNow: (id: string, promptId: string) => invoke<void>("code_send_now", { id, promptId }),
  getCodeTranscript: (id: string) => invoke<string | null>("get_code_transcript", { id }),
  saveCodeTranscript: (id: string, json: string) => invoke<void>("save_code_transcript", { id, json }),
  /** With `sessionId`, lists files on the node hosting that session. */
  listProjectFiles: (cwd: string, sessionId?: string) => invoke<string[]>("list_project_files", { cwd, sessionId }),
  /** A picture the reply linked, read on the computer that holds the session. A data URL. */
  codeSessionImage: (id: string, path: string) => invoke<string>("code_session_image", { id, path }),

  listMcpServers: () => invoke<McpServer[]>("list_mcp_servers"),
  saveMcpServer: (input: { id?: string; name: string; enabled: boolean; config: McpConfig }) =>
    invoke<McpServer>("save_mcp_server", { input }),
  deleteMcpServer: (id: string) => invoke<void>("delete_mcp_server", { id }),
  listPermissionRules: () => invoke<PermissionRule[]>("list_permission_rules"),
  addPermissionRule: (cwd: string, toolKind: string, pattern: string) =>
    invoke<PermissionRule>("add_permission_rule", { cwd, toolKind, pattern }),
  deletePermissionRule: (id: string) => invoke<void>("delete_permission_rule", { id }),

  listPrompts: () => invoke<Prompt[]>("list_prompts"),
  savePrompt: (input: { id?: string; name: string; content: string }) => invoke<Prompt>("save_prompt", input),
  deletePrompt: (id: string) => invoke<void>("delete_prompt", { id }),

  usageReport: (days: number) => invoke<UsageReport>("usage_report", { days }),
  quickSubmit: (text: string) => invoke<void>("quick_submit", { text }),
  quickDismiss: () => invoke<void>("quick_dismiss"),
  codeCancel: (id: string) => invoke<void>("code_cancel", { id }),
  codeSetModel: (id: string, modelId: string) => invoke<void>("code_set_model", { id, modelId }),
  codeSetMode: (id: string, modeId: string) => invoke<void>("code_set_mode", { id, modeId }),
  codeSetPermissionMode: (id: string, mode: "default" | "auto" | "always-approve") =>
    invoke<void>("code_set_permission_mode", { id, mode }),
  codeSetAgent: (id: string, agent: AgentId, modelRef: string | null) =>
    invoke<void>("code_set_agent", { id, agent, modelRef }),
  codeSetConfigOption: (id: string, configId: string, value: string) =>
    invoke<void>("code_set_config_option", { id, configId, value }),
  /** Sign the agent in; resolves once its session is up. A browser sign-in can take minutes. */
  codeAuthenticate: (id: string, methodId: string, apiKey?: string) =>
    invoke<void>("code_authenticate", { id, methodId, apiKey: apiKey ?? null }),
  codeRewind: (id: string, ordinal: number, preview: string, revertFiles: boolean) =>
    invoke<void>("code_rewind", { id, ordinal, preview, revertFiles }),
  codeUsage: (id: string) => invoke<UsageView>("code_usage", { id }),
  planLimits: () => invoke<PlanLimits | null>("plan_limits"),
  planHistory: () => invoke<PlanSample[]>("plan_history"),
  wireLogStatus: () => invoke<WireLogStatus>("wire_log_status"),
  /** Turn the wire log on (for 30 minutes) or off. */
  wireLogSet: (on: boolean) => invoke<WireLogStatus>("wire_log_set", { on }),
  /** Recorded frames, oldest first; `last`: only the newest that many. */
  wireLogFrames: (session?: string, last?: number) =>
    invoke<WireFrame[]>("wire_log_frames", { session: session ?? null, last: last ?? null }),
  wireLogClear: () => invoke<WireLogStatus>("wire_log_clear"),
  /** Write the log to a file; resolves to its path. */
  wireLogSave: () => invoke<string>("wire_log_save"),
  codePermissionRespond: (requestId: string, optionId: string | null, sessionId?: string) =>
    invoke<void>("code_permission_respond", { requestId, optionId, id: sessionId }),
  codeAttach: (id: string) => invoke<CodeAttach>("code_attach", { id }),
  /** Move a session to `target` into folder `cwd` there; resolves to a note for the user, if any. */
  codeMove: (id: string, target: string, cwd: string) => invoke<string | null>("code_move", { id, target, cwd }),
  codeTakeOver: (id: string, cwd: string) => invoke<string | null>("code_take_over", { id, cwd }),
  /** Session id -> when the checkpoint this node holds was taken. */
  codeCheckpoints: () => invoke<Record<string, number>>("code_checkpoints"),

  meshStatus: () => invoke<MeshStatus>("mesh_status"),
  meshCreate: (passphrase: string, name: string) => invoke<void>("mesh_create", { passphrase, name }),
  meshJoin: (url: string, passphrase: string, name: string) => invoke<void>("mesh_join", { url, passphrase, name }),
  meshCancelJoin: () => invoke<void>("mesh_cancel_join"),
  meshApprove: (nodeId: string) => invoke<void>("mesh_approve", { nodeId }),
  meshReject: (nodeId: string) => invoke<void>("mesh_reject", { nodeId }),
  meshRemove: (nodeId: string) => invoke<void>("mesh_remove", { nodeId }),
  meshLeave: () => invoke<void>("mesh_leave"),
  meshSetConfig: (cfg: { name?: string; listen?: string; advertise?: string[] }) => invoke<void>("mesh_set_config", cfg),
  meshSetPassphrase: (passphrase: string) => invoke<void>("mesh_set_passphrase", { passphrase }),
  meshAddAddress: (url: string) => invoke<void>("mesh_add_address", { url }),
  meshChatStreams: () => invoke<RemoteStream[]>("mesh_chat_streams"),

  /** With `sessionId`, the shell runs on the computer hosting that session. */
  /** With `authMethod`, the terminal runs the session's agent's sign-in command for that method. */
  termOpen: (
    id: string,
    cwd: string,
    cols: number,
    rows: number,
    onEvent: (e: TermEvent) => void,
    sessionId?: string,
    authMethod?: string,
  ) => {
    const ch = new Channel<TermEvent>();
    ch.onmessage = onEvent;
    return invoke<void>("term_open", { id, cwd, cols, rows, onEvent: ch, sessionId, authMethod: authMethod ?? null });
  },
  termWrite: (id: string, data: string) => invoke<void>("term_write", { id, data }),
  termResize: (id: string, cols: number, rows: number) => invoke<void>("term_resize", { id, cols, rows }),
  termClose: (id: string) => invoke<void>("term_close", { id }),

  writeTextFile: (path: string, contents: string) => invoke<void>("write_text_file", { path, contents }),
  /** Native app only: whether a path from a transcript exists here (relative ones under `cwd`). */
  localPathExists: (path: string, cwd: string | null) => invoke<boolean>("local_path_exists", { path, cwd }),
  /** Native app only: open that path in the OS's default app, or reveal it if it would run. */
  openLocalPath: (path: string, cwd: string | null) => invoke<void>("open_local_path", { path, cwd }),
  /** Native app only: a newer release it can install itself, or null. */
  updateCheck: () => invoke<UpdateInfo | null>("update_check"),
  /** Native app only: install what `updateCheck` found and restart. */
  updateInstall: () => invoke<void>("update_install"),
  /** Folders under `path` (default: home) on this computer, for choosing one without a native dialog. */
  /** With `host`, the folders on that computer. */
  listDirs: (path: string | null, host?: string | null) => invoke<DirListing>("list_dirs", { path, host: host ?? null }),
  /** The Patchbay version and OS of this computer, or with `host` of that one. */
  hostInfo: (host?: string | null) => invoke<HostInfo | null>("host_info", { host: host ?? null }),
  /** With `host`, what that computer offers. */
  newSessionOptions: (host?: string | null) => invoke<NewSessionOptions>("new_session_options", { host: host ?? null }),
};

export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}
