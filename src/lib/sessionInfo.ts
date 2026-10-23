// What a Code session is, as plain rows: the raw ids, the agent and its build, the model, where it
// runs. One list feeds the details dialog and the "copy all" text (modelled on Claude Code's
// `/status`), so the two can't drift. Pure: the caller gathers the inputs.
import type { AcpConfigChoice, AcpConfigOption, AgentInfo, CodeSession } from "./ipc";
import type { CodeState } from "./store";

export interface InfoRow {
  label: string;
  value: string;
  /** An id or path: shown in monospace, breakable anywhere. */
  mono?: boolean;
  /** Offer a copy button for just this value. */
  copy?: boolean;
}

export interface InfoSection {
  title: string;
  rows: InfoRow[];
}

export interface SessionInfoInput {
  session: CodeSession;
  /** Live state; absent while nothing has attached to the session yet. */
  state?: Pick<
    CodeState,
    "status" | "acpSessionId" | "agentInfo" | "models" | "modes" | "configOptions" | "capabilities" | "extensions" | "usage" | "approvalGaps"
  > | null;
  /** The agent's label, e.g. "Claude Code". */
  agentLabel: string;
  /** This session's agent as installed on the computer that runs it, when known. */
  build?: Pick<AgentInfo, "program" | "version" | "source"> | null;
  /** Name of the computer running the agent; null for this one. */
  hostName: string | null;
  /** True when that computer is the one showing this. */
  isLocalHost: boolean;
}

const APPROVALS: Record<CodeSession["permissionMode"], string> = {
  default: "Ask first",
  auto: "Auto edits",
  "always-approve": "Approve all",
};

const BUILD_SOURCES: Record<NonNullable<AgentInfo["source"]>, string> = {
  override: "set in Settings",
  builtin: "built into Patchbay",
  path: "on PATH",
  npx: "downloaded by npx",
};

const STATUSES: Record<NonNullable<SessionInfoInput["state"]>["status"], string> = {
  idle: "Not running",
  starting: "Starting",
  ready: "Running",
  exited: "Stopped",
};

const iso = (ms: number) => new Date(ms).toISOString();

function choiceName(options: AcpConfigChoice[], value: string): string | null {
  for (const o of options) {
    if ("group" in o) {
      const inner = choiceName(o.options, value);
      if (inner) return inner;
    } else if (o.value === value) return o.name;
  }
  return null;
}

/** "Name (id)" so a person and a script can both use the line; just the id when they match. */
function named(name: string | null | undefined, id: string): string {
  return name && name !== id ? `${name} (${id})` : id;
}

function tokens(n: number): string {
  return n.toLocaleString("en-US");
}

function modelRow(i: SessionInfoInput): string | null {
  const { session, state } = i;
  const current = state?.models?.currentModelId;
  if (current) {
    const name = state?.models?.availableModels.find((m) => m.modelId === current)?.name;
    return named(name, current);
  }
  return session.agentModel ?? session.modelRef;
}

function featureList(i: SessionInfoInput): string[] {
  const caps = i.state?.capabilities;
  const ext = i.state?.extensions;
  const out: string[] = [];
  if (caps?.loadSession) out.push("resume");
  if (caps?.promptCapabilities?.image) out.push("images");
  if (caps?.promptCapabilities?.audio) out.push("audio");
  if (caps?.promptCapabilities?.embeddedContext) out.push("embedded context");
  if (caps?.mcpCapabilities?.http) out.push("MCP over HTTP");
  if (caps?.mcpCapabilities?.sse) out.push("MCP over SSE");
  if (ext?.steering) out.push("mid-turn steering");
  if (ext?.rewind) out.push("rewind");
  if (ext?.statusLine) out.push("status line");
  return out;
}

function optionRow(o: AcpConfigOption): InfoRow {
  return { label: o.name, value: named(choiceName(o.options, o.currentValue), o.currentValue) };
}

export function sessionInfoSections(i: SessionInfoInput): InfoSection[] {
  const { session, state } = i;
  const agentSession = state?.acpSessionId ?? session.acpSessionId;
  const info = state?.agentInfo;
  const agentVersion = info?.version ?? i.build?.version ?? null;
  const agentName = info?.title ?? info?.name;

  const sessionRows: (InfoRow | null)[] = [
    { label: "Title", value: session.title },
    { label: "Patchbay session ID", value: session.id, mono: true, copy: true },
    {
      label: "Agent session ID",
      value: agentSession ?? "none yet: the agent hasn't started this session",
      mono: !!agentSession,
      copy: !!agentSession,
    },
    { label: "Agent", value: [i.agentLabel, agentVersion].filter(Boolean).join(" ") },
    agentName && agentName !== i.agentLabel ? { label: "Agent reports", value: agentName } : null,
    { label: "Status", value: STATUSES[state?.status ?? "idle"] },
    modelRow(i) ? { label: "Model", value: modelRow(i)! } : null,
    state?.modes
      ? {
          label: "Mode",
          value: named(
            state.modes.availableModes.find((m) => m.id === state.modes!.currentModeId)?.name,
            state.modes.currentModeId,
          ),
        }
      : null,
    ...(state?.configOptions ?? []).filter((o) => o.type === "select").map(optionRow),
    { label: "Approvals", value: APPROVALS[session.permissionMode] ?? APPROVALS.default },
    { label: "Created", value: iso(session.createdAt) },
    { label: "Last active", value: iso(session.updatedAt) },
    session.archivedAt != null ? { label: "Archived", value: iso(session.archivedAt) } : null,
  ];

  const where = i.isLocalHost ? "This computer" : (i.hostName ?? session.hostNode ?? "Another computer");
  const environmentRows: (InfoRow | null)[] = [
    { label: "Folder", value: session.cwd, mono: true, copy: true },
    { label: "Runs on", value: where },
    session.hostNode ? { label: "Host node ID", value: session.hostNode, mono: true, copy: true } : null,
    session.epoch > 0 ? { label: "Moves between computers", value: String(session.epoch) } : null,
    i.build?.program
      ? {
          label: "Agent program",
          value: i.build.source ? `${i.build.program} (${BUILD_SOURCES[i.build.source]})` : i.build.program,
          mono: true,
          copy: true,
        }
      : null,
  ];

  const sections: InfoSection[] = [
    { title: "Session", rows: sessionRows.filter((r): r is InfoRow => r != null) },
    { title: "Environment", rows: environmentRows.filter((r): r is InfoRow => r != null) },
  ];

  const features = featureList(i);
  const agentRows: InfoRow[] = [];
  if (features.length > 0) agentRows.push({ label: "Supports", value: features.join(", ") });
  if ((state?.approvalGaps.length ?? 0) > 0) {
    agentRows.push({ label: "Runs without asking", value: state!.approvalGaps.join(" ") });
  }
  if (agentRows.length > 0) sections.push({ title: "Agent", rows: agentRows });

  const usage = state?.usage;
  if (usage) {
    const rows: InfoRow[] = [];
    if (usage.contextTokens != null) {
      rows.push({
        label: "Context",
        value: usage.contextWindow
          ? `${tokens(usage.contextTokens)} of ${tokens(usage.contextWindow)} tokens`
          : `${tokens(usage.contextTokens)} tokens`,
      });
    }
    const t = usage.totals;
    if (t.input + t.output > 0) {
      rows.push({ label: "Tokens", value: `${tokens(t.input)} in (${tokens(t.cachedRead)} cached), ${tokens(t.output)} out` });
    }
    if (t.costUsd != null) rows.push({ label: "Cost", value: `$${t.costUsd.toFixed(4)}` });
    if (rows.length > 0) sections.push({ title: "Usage", rows });
  }

  return sections;
}

/** The sections as text to paste into a bug report or a shell: `/status`'s layout. */
export function sessionInfoText(sections: InfoSection[], at: number = Date.now()): string {
  const body = sections.map((s) => [`${s.title}:`, ...s.rows.map((r) => `- ${r.label}: ${r.value}`)].join("\n"));
  return [`Patchbay session info (${iso(at)})`, ...body].join("\n\n");
}
