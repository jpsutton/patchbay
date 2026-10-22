import { memo, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  AGENT_LABELS,
  api,
  errorText,
  estimateCost,
  type AcpConfigOption,
  type AcpToolCall,
  type AgentId,
  type AgentInfo,
  type Attachment,
  type CodeSession,
} from "../lib/ipc";
import {
  codeWorking,
  flushCodeEvents,
  FOREGROUND_IDLE_MS,
  harnessTags,
  hasActiveForegroundTools,
  memberName,
  memberOnline,
  remoteHost,
  useStore,
  type CodeState,
  type Entry,
} from "../lib/store";
import { HostBadge, MoveDialog, OfflineHostBanner } from "./HostControls";
import { Composer } from "../components/Composer";
import { Markdown } from "../components/Markdown";
import { EditMessage } from "../components/EditMessage";
import { UsageBar } from "../components/UsageBar";
import { TerminalPane } from "../terminal/TerminalPane";
import { PermissionCard, PlanDocCard, planDocOf, ToolCallCard, ToolGroup } from "./ToolCallCard";
import { SignInCard } from "./SignInCard";
import { TranscriptRows, type TranscriptRow } from "./TranscriptRows";
import { DiffView, diffStats } from "./DiffView";
import { changesKey, collectChanges, normalizePath, type FileChange } from "./changes";
import { todoPlanOf } from "../lib/todoPlan";
import { CopyButton, copyText } from "../components/CopyButton";
import { ExportMenu } from "../components/ExportMenu";
import { codeToJson, codeToMarkdown, saveExport } from "../lib/export";
import { useViewAction } from "../lib/shortcuts";
import { HeaderButton } from "../components/HeaderButton";
import { useDragRegion, WindowControls } from "../components/WindowControls";
import { isRemoteApp } from "../lib/platform";
import {
  CheckIcon,
  ChevronRightIcon,
  CopyIcon,
  FileDiffIcon,
  MonitorUpIcon,
  PaperclipIcon,
  PencilIcon,
  PlusIcon,
  RestartIcon,
  TerminalIcon,
  XIcon,
} from "../components/icons";
import { ChipRows, ChipSelect, ChipText } from "../components/ChipSelect";
import { AgentImage } from "../components/AgentImage";
import { ModelRing } from "../lib/cable";
import { JumpToBottom, ScrollFade, useStickyScroll } from "../lib/useStickyScroll";


/**
 * Start a Code session in `dir` with `agent`, or the default agent from Settings; with `host`,
 * in that computer's folder `dir`.
 */
export async function newSessionIn(dir: string, agent?: AgentId, host?: string | null) {
  const st = useStore.getState();
  try {
    // Another computer starts its own default agent: this one's may not be installed there.
    const s = await api.createCodeSession(dir, st.defaultModelRef, agent ?? (host ? undefined : st.prefs.defaultAgent), host);
    await st.refreshCodeSessions();
    st.setMode("code");
    st.selectCodeSession(s.id);
  } catch (e) {
    st.setToast(errorText(e));
  }
}

/** Choices of a select config option, flattening groups. */
function configChoices(o: AcpConfigOption) {
  return o.options.flatMap((c) => ("options" in c ? c.options : [c]));
}

/** Agents installed here, or on computer `host`; asked once per window and computer. */
const agentLists = new Map<string, Promise<AgentInfo[]>>();
export function installedAgents(host?: string | null): Promise<AgentInfo[]> {
  const key = host ?? "";
  let list = agentLists.get(key);
  if (!list) {
    list = api.listAgents(host).catch(() => {
      agentLists.delete(key);
      return [];
    });
    agentLists.set(key, list);
  }
  return list;
}

/**
 * The agent of a session, switchable in its composer. Once the session has messages, the new agent
 * starts fresh with the conversation so far as context (`handoff::switch_prompt`), so it asks
 * first. The default isn't touched.
 */
function SessionAgentPicker({
  session,
  host,
  started,
  disabled,
}: {
  session: CodeSession;
  /** The computer running the session, when that isn't this device (always, on a phone). */
  host: string | null;
  started: boolean;
  disabled: boolean;
}) {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  useEffect(() => {
    let live = true;
    installedAgents(host).then((a) => live && setAgents(a));
    return () => {
      live = false;
    };
  }, [host]);
  const usable = agents.filter((a) => a.available || a.id === session.agent);
  const rows = useContext(ChipRows);
  const label = (id: string) => agents.find((a) => a.id === id)?.label ?? AGENT_LABELS[id as AgentId] ?? id;
  if (usable.length < 2) return rows ? <ChipText label="Agent">{label(session.agent)}</ChipText> : null;
  return (
    <ChipSelect
      className="max-w-44"
      value={session.agent}
      disabled={disabled}
      title="Agent for this session"
      aria-label="Agent for this session"
      label="Agent"
      onChange={async (e) => {
        const st = useStore.getState();
        const next = e.target.value as AgentId;
        if (started) {
          const { ok } = await st.askConfirm({
            title: `Switch to ${label(next)}?`,
            body:
              `${label(next)} starts fresh and gets this conversation as text, with the tools ` +
              `${label(session.agent)} used. It doesn't get ${label(session.agent)}'s internal state, and ` +
              "earlier messages can't be edited or rewound afterwards. Files aren't touched.",
            confirmLabel: "Switch agent",
          });
          if (!ok) return;
        }
        try {
          await api.codeSetAgent(session.id, next, st.defaultModelRef);
          // The transcript was folded into earlier history: show it from the saved copy.
          if (started) st.resetCode(session.id);
          await st.refreshCodeSessions();
          // Another computer runs it: attach, which starts the new agent there and follows it.
          await (host ? st.attachAgent(session.id) : st.startAgent(session.id));
        } catch (err) {
          st.setToast(errorText(err));
        }
      }}
    >
      {usable.map((a) => (
        <option key={a.id} value={a.id}>
          {a.label}
        </option>
      ))}
    </ChipSelect>
  );
}

/** Picks the agent for new sessions on the empty Code screen; saves it as the default. */
function AgentPicker() {
  const prefs = useStore((s) => s.prefs);
  const updatePrefs = useStore((s) => s.updatePrefs);
  const [agents, setAgents] = useState<AgentInfo[] | null>(null);
  useEffect(() => {
    api.listAgents().then(setAgents, () => setAgents(null));
  }, []);
  if (!agents) return null;
  const cur = agents.find((a) => a.id === prefs.defaultAgent);
  return (
    <div className="mt-3 flex flex-col items-center gap-1.5 text-[13px]">
      <label className="flex items-center gap-2">
        <span className="text-muted">Agent</span>
        <ChipSelect
          value={prefs.defaultAgent}
          aria-label="Coding agent for new sessions"
          onChange={(e) => updatePrefs({ defaultAgent: e.target.value as AgentId })}
        >
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.label}
              {a.available ? "" : " (not installed)"}
            </option>
          ))}
        </ChipSelect>
      </label>
      {cur && !cur.available && <div className="max-w-md text-[12px] text-danger">{cur.error ?? cur.installHint}</div>}
    </div>
  );
}

function Thought({ text }: { text: string }) {
  const [open, setOpen] = useState(() => useStore.getState().prefs.expandReasoning);
  return (
    <div className="text-xs">
      <button
        className="flex h-[30px] items-center gap-2 rounded-lg border border-line px-2.5 text-muted hover:bg-panel-2/60 hover:text-fg"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
      >
        <span className={`flex transition-transform ${open ? "rotate-90" : ""}`}>
          <ChevronRightIcon size={12} />
        </span>
        Thinking
      </button>
      {open && (
        <div className="mt-2 whitespace-pre-wrap border-l-2 border-line pl-3 leading-relaxed text-muted">{text}</div>
      )}
    </div>
  );
}

/** A message the agent's harness wrote as the user: dimmed and collapsed, named by its elements. */
function HarnessEntry({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const tags = useMemo(() => harnessTags(text) ?? [], [text]);
  return (
    <div className="text-xs opacity-70">
      <button
        className="flex h-[26px] max-w-full items-center gap-2 rounded-lg px-1.5 text-faint hover:bg-panel-2/60 hover:text-muted"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        title="Added by the agent, not typed by you"
      >
        <span className={`flex transition-transform ${open ? "rotate-90" : ""}`}>
          <ChevronRightIcon size={12} />
        </span>
        <span className="truncate font-mono">{tags.join(", ")}</span>
      </button>
      {open && (
        <div className="mt-1 max-h-80 overflow-auto whitespace-pre-wrap border-l-2 border-line pl-3 font-mono leading-relaxed text-faint">
          {text}
        </div>
      )}
    </div>
  );
}

function UserEntry({
  e,
  editing,
  setEditing,
  onEdit,
}: {
  e: Extract<Entry, { kind: "user" }>;
  editing: boolean;
  setEditing: (on: boolean) => void;
  onEdit?: (text: string, atts: Attachment[]) => Promise<boolean>;
}) {
  if (editing && onEdit) {
    return (
      <div className="flex flex-col">
        <EditMessage
          text={e.text}
          attachments={e.attachments ?? []}
          onCancel={() => setEditing(false)}
          onSubmit={async (t, a) => {
            // Stay in edit mode if the user backs out of the confirmation.
            if (await onEdit(t, a)) setEditing(false);
          }}
        />
      </div>
    );
  }
  return (
    <div className="group flex flex-col items-end gap-1.5">
      {e.attachments && e.attachments.length > 0 && (
        <div className="flex items-center gap-1.5 text-xs text-muted">
          <PaperclipIcon size={13} />
          {e.attachments.map((a) => a.name).join(", ")}
        </div>
      )}
      {e.text && (
        <div className="max-w-[82%] whitespace-pre-wrap rounded-[16px_16px_4px_16px] border border-line bg-panel-2 px-3.5 py-2.5 leading-6">
          {e.text}
        </div>
      )}
      <div className="flex gap-0.5 text-muted opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        {onEdit && (
          <button
            className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 hover:text-fg"
            onClick={() => setEditing(true)}
            title="Edit"
            aria-label="Edit message"
          >
            <PencilIcon size={14} />
          </button>
        )}
        {e.text && <CopyButton text={e.text} icon={<CopyIcon size={14} />} />}
      </div>
    </div>
  );
}

function AgentEntry({ text }: { text: string }) {
  return (
    <div className="group">
      <Markdown text={text} />
      <div className="mt-1 flex gap-0.5 text-muted opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        <CopyButton text={text} label="Copy Markdown" icon={<CopyIcon size={14} />} />
      </div>
    </div>
  );
}

type Permission = CodeState["permissions"][number];

/**
 * One transcript entry. Memoized, and given only what it shows, so a long transcript re-renders
 * just the entries an update touched. `onEditing` and `onEdit` must be stable.
 */
const EntryView = memo(function EntryView({
  e,
  i,
  tc,
  perm,
  autoApproved,
  cwd,
  editing,
  canEdit,
  onEditing,
  onEdit,
}: {
  e: Entry;
  /** The entry's index, passed back to `onEditing` and `onEdit`. */
  i: number;
  tc?: AcpToolCall;
  perm?: Permission;
  autoApproved?: string;
  cwd: string;
  editing: boolean;
  canEdit: boolean;
  onEditing?: (i: number | null) => void;
  onEdit?: (i: number, text: string, atts: Attachment[]) => Promise<boolean>;
}) {
  switch (e.kind) {
    case "user":
      return (
        <UserEntry
          e={e}
          editing={editing}
          setEditing={(on) => onEditing?.(on ? i : null)}
          onEdit={canEdit && onEdit ? (t, a) => onEdit(i, t, a) : undefined}
        />
      );
    case "agent":
      return <AgentEntry text={e.text} />;
    case "image":
      return <AgentImage data={e.data} mimeType={e.mimeType} alt="Image from the agent" />;
    case "thought":
      return <Thought text={e.text} />;
    case "tool": {
      if (perm) return <PermissionCard requestId={perm.requestId} request={perm.request} live={tc} cwd={cwd} />;
      const doc = tc && planDocOf(tc);
      if (doc) return <PlanDocCard name={doc.name} text={doc.text} />;
      return tc ? <ToolCallCard tc={tc} autoApproved={autoApproved} /> : null;
    }
    case "error":
      return (
        <div className="whitespace-pre-wrap rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 font-mono text-xs text-danger">
          {e.text}
        </div>
      );
    case "info":
      return <div className="text-center text-[12px] text-faint">{e.text}</div>;
    case "harness":
      return <HarnessEntry text={e.text} />;
    case "cancelled":
      return <div className="text-center font-mono text-[11px] text-faint">Turn cancelled</div>;
    case "unrecognized":
      return <UnrecognizedEntry text={e.text} raw={e.raw} count={e.count} />;
    default:
      return null;
  }
});

/** A dim line for something the agent sent that Patchbay can't show; opens to the raw payload. */
function UnrecognizedEntry({ text, raw, count }: { text: string; raw: string; count?: number }) {
  return (
    <details className="group text-[12px] text-faint">
      <summary className="cursor-pointer list-none text-center hover:text-muted">
        {text}
        {count && count > 1 ? ` (×${count})` : ""}
        <span className="ml-1 group-open:hidden">· show</span>
      </summary>
      <pre className="mt-1.5 max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px] leading-5 text-muted">
        {raw}
      </pre>
    </details>
  );
}

function PlanPanel({ plan }: { plan: NonNullable<CodeState["plan"]> }) {
  if (plan.length === 0) return null;
  const done = plan.filter((p) => p.status === "completed").length;
  // Cancelled steps stay listed but don't count toward the total.
  const total = plan.filter((p) => p.status !== "cancelled").length;
  return (
    <section aria-label="Plan" className="flex flex-col gap-2 rounded-xl border border-line bg-panel/60 px-3.5 py-3 text-[13px]">
      <div className="flex items-center font-mono text-[11px] text-faint">
        <span className="uppercase tracking-[0.06em]">Plan</span>
        <span className="flex-1" />
        <span>
          {done} of {total}
        </span>
      </div>
      <div
        className="h-[3px] overflow-hidden rounded-full bg-panel-2"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={done}
        aria-label="Plan progress"
      >
        <div className="h-full bg-accent transition-[width]" style={{ width: `${total ? (done / total) * 100 : 0}%` }} />
      </div>
      <ol className="flex flex-col gap-1">
        {plan.map((p, i) => (
          <li key={i} className="flex items-start gap-2.5">
            <span className="flex h-5 w-3.5 shrink-0 items-center justify-center">
              {p.status === "completed" ? (
                <span className="flex text-ok">
                  <CheckIcon size={14} strokeWidth={2.4} />
                </span>
              ) : p.status === "in_progress" ? (
                <span className="signal-pulse h-2 w-2 rounded-full bg-accent" />
              ) : p.status === "cancelled" ? (
                <span className="flex text-faint" title="Cancelled">
                  <XIcon size={13} strokeWidth={2.2} />
                </span>
              ) : (
                <span className="h-2 w-2 rounded-full border-[1.5px] border-faint/60" />
              )}
            </span>
            <span
              className={
                p.status === "completed"
                  ? "text-muted line-through decoration-faint/40"
                  : p.status === "cancelled"
                    ? "text-faint line-through decoration-faint/40"
                  : p.status === "in_progress"
                    ? "text-fg"
                    : "text-muted"
              }
            >
              {p.content}
            </span>
          </li>
        ))}
      </ol>
    </section>
  );
}

/** What a backgrounded command is doing: the agent's description of it, else its title. */
function backgroundLabel(tc: AcpToolCall): string {
  const raw = (tc.rawInput ?? {}) as Record<string, unknown>;
  const label = typeof raw.description === "string" && raw.description.trim() ? raw.description : tc.title;
  return (label || "a command").split("\n")[0];
}

type Run = { i: number; e: Entry } | { tools: { i: number; e: Entry }[] };

/**
 * Consecutive tool calls form one run, drawn as a single box with hairlines between rows. A call
 * waiting on a permission stays on its own, so its card can't hide inside a group, and so does a
 * plan document, which reads as part of the conversation. A saved to-do write shows as the plan
 * panel instead of a card (only for the agents whose plan it is: `agent`).
 */
function runsOf(entries: Entry[], s: Pick<CodeState, "toolCalls" | "permissions">, agent?: AgentId): Run[] {
  const out: Run[] = [];
  entries.forEach((e, i) => {
    const tc = e.kind === "tool" ? s.toolCalls[e.id] : undefined;
    if (tc && todoPlanOf(tc, agent)) return;
    const groupable =
      !!tc && !planDocOf(tc) && !s.permissions.some((p) => p.request.toolCall.toolCallId === tc.toolCallId);
    const last = out[out.length - 1];
    if (groupable && last && "tools" in last) last.tools.push({ i, e });
    else if (groupable) out.push({ tools: [{ i, e }] });
    else out.push({ i, e });
  });
  return out;
}

/** A run as one transcript row: `key` keeps each entry's row (and measured height) across renders. */
function runRow(r: Run, key: string, dim: boolean, one: (e: Entry, i: number) => ReactNode): TranscriptRow {
  return "tools" in r
    ? { key: `${key}g${r.tools[0].i}`, dim, render: () => <ToolGroup>{r.tools.map(({ e, i }) => one(e, i))}</ToolGroup> }
    : { key: `${key}${r.i}`, dim, render: () => one(r.e, r.i) };
}

const STATUS = {
  added: { letter: "A", label: "Added", tone: "text-ok" },
  modified: { letter: "M", label: "Modified", tone: "text-accent-ink" },
  deleted: { letter: "D", label: "Deleted", tone: "text-danger" },
} as const;

function ChangesPanel({ changes, cwd, onClose }: { changes: FileChange[]; cwd: string; onClose: () => void }) {
  const [sel, setSel] = useState<string | null>(null);
  const cur = changes.find((c) => c.path === sel) ?? changes[0];
  const root = normalizePath(cwd, "/");
  const prefix = root.endsWith("/") ? root : root + "/";
  const rel = (p: string) => (p.startsWith(prefix) && p.length > prefix.length ? p.slice(prefix.length) : p);
  const stats = changes.map((c) =>
    c.parts
      .map((p) => diffStats(p.oldText, p.newText))
      .reduce((a, b) => ({ add: a.add + b.add, del: a.del + b.del }), { add: 0, del: 0 }),
  );
  const total = stats.reduce((a, b) => ({ add: a.add + b.add, del: a.del + b.del }), { add: 0, del: 0 });
  return (
    <div className="flex h-full flex-col">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line pl-4 pr-2">
        <span className="flex-1 text-[13px] font-medium">Changes</span>
        {changes.length > 0 && (
          <span className="meta">
            <span className="text-ok">+{total.add}</span> <span className="text-danger">−{total.del}</span>
          </span>
        )}
        <button
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
          onClick={onClose}
          aria-label="Close changes"
          title="Close"
        >
          <XIcon size={14} />
        </button>
      </div>
      {changes.length === 0 && <div className="p-4 text-xs text-faint">No file changes yet.</div>}
      {changes.length > 0 && (
      <ul className="flex max-h-48 shrink-0 flex-col gap-px overflow-y-auto border-b border-line p-1.5">
        {changes.map((c, i) => {
          const st = stats[i];
          const status = STATUS[c.status];
          return (
            <li key={c.path}>
            <button
              className={`flex h-8 w-full items-center gap-2 rounded-md px-2.5 text-left hover:bg-panel-2 ${
                cur?.path === c.path ? "bg-panel-2 text-fg" : "text-fg/75"
              }`}
              onClick={() => setSel(c.path)}
            >
              <span className={`w-3 font-mono text-[11px] ${status.tone}`} title={status.label}>
                {status.letter}
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{rel(c.path)}</span>
              <span className="meta">
                <span className="text-ok">+{st.add}</span> <span className="text-danger">−{st.del}</span>
              </span>
            </button>
            </li>
          );
        })}
      </ul>
      )}
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-auto p-2">
        {cur?.parts.map((p, i) => (
          <DiffView
            key={`${cur.path}:${i}`}
            path={rel(cur.path)}
            oldText={p.oldText}
            newText={p.newText}
            oldStart={p.oldStart}
            newStart={p.newStart}
            maxHeight={10000}
          />
        ))}
      </div>
    </div>
  );
}

/** `leading`: something before the title (the phone app's back button). */
export function CodeView({ leading }: { leading?: ReactNode } = {}) {
  const drag = useDragRegion();
  const remote = isRemoteApp();
  const activeId = useStore((s) => s.activeCodeSessionId);
  const sessions = useStore((s) => s.codeSessions);
  const state = useStore((s) => (activeId ? s.code[activeId] : undefined));
  const models = useStore((s) => s.models);
  const pushCodeEntry = useStore((s) => s.pushCodeEntry);
  const setCodeBusy = useStore((s) => s.setCodeBusy);
  const clearCodeRestore = useStore((s) => s.clearCodeRestore);
  const refreshCodeSessions = useStore((s) => s.refreshCodeSessions);
  const setToast = useStore((s) => s.setToast);
  const askConfirm = useStore((st) => st.askConfirm);
  const truncateCode = useStore((st) => st.truncateCode);
  const session = sessions.find((s) => s.id === activeId) ?? null;

  const [showTerm, setShowTerm] = useState(false);
  const [showChanges, setShowChanges] = useState(false);
  const [termMounted, setTermMounted] = useState<Record<string, boolean>>({});
  const [editingIdx, setEditingIdx] = useState<number | null>(null);
  const [moving, setMoving] = useState<"move" | "takeOver" | null>(null);
  /** Forces a re-render when the post-foreground idle window expires. */
  const [, setIdleTick] = useState(0);
  const sticky = useStickyScroll([state?.entries, state?.toolCalls, state?.permissions, state?.auth]);
  const mesh = useStore((st) => st.mesh);
  const meshLoaded = !!mesh;

  const start = useStore((st) => st.startAgent);
  const attach = useStore((st) => st.attachAgent);

  useEffect(() => {
    setEditingIdx(null);
    sticky.pin();
    if (!activeId) return;
    const cur = useStore.getState().code[activeId];
    if (cur && cur.status !== "idle") return;
    const sess = useStore.getState().codeSessions.find((x) => x.id === activeId);
    // Until the mesh state is known, a session might be running on another computer.
    if (sess?.hostNode && !meshLoaded && !remote) return;
    // Running on another computer: watch it there, never restart it just because we looked.
    // A phone hosts no session, so it always watches, even one its list hasn't caught up on yet.
    if (remote || remoteHost(sess, useStore.getState().mesh)) {
      attach(activeId);
      return;
    }
    api.codeIsRunning(activeId).then((running) => {
      // A running agent without UI state (e.g. after a reload) is restarted so history replays.
      if (!running || !cur) start(activeId);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId, meshLoaded]);

  const earlierRuns = useMemo(
    // Earlier turns ran under other agents: their cards all show.
    () => (state?.earlier ? runsOf(state.earlier.entries, { toolCalls: state.earlier.toolCalls, permissions: [] }) : []),
    [state?.earlier],
  );
  const historyRuns = useMemo(
    () =>
      state?.history
        ? runsOf(state.history.entries, { toolCalls: state.history.toolCalls, permissions: [] }, session?.agent)
        : [],
    [state?.history, session?.agent],
  );
  const entryRuns = useMemo(
    () => (state ? runsOf(state.entries, state, session?.agent) : []),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [state?.entries, state?.toolCalls, state?.permissions, session?.agent],
  );
  // Stable for the memoized entries; it calls the latest `editAndResend`, set on each render.
  const editRef = useRef<(i: number, text: string, atts: Attachment[]) => Promise<boolean>>(null);
  const onEdit = useCallback(
    (i: number, text: string, atts: Attachment[]) => editRef.current?.(i, text, atts) ?? Promise.resolve(false),
    [],
  );

  // The header's badge needs the count even with the panel shut, so this always runs; the key
  // keeps it from redoing the work when only text or non-diff tool output changed.
  const changesCache = useRef<{ key: unknown[]; value: FileChange[] } | null>(null);
  const changes = useMemo(() => {
    if (!state || !session) return [];
    const key = [session.cwd, ...changesKey(state.entries, state.toolCalls)];
    const hit = changesCache.current;
    if (hit && hit.key.length === key.length && hit.key.every((k, i) => k === key[i])) return hit.value;
    const value = collectChanges(state.entries, state.toolCalls, session.cwd);
    changesCache.current = { key, value };
    return value;
  }, [state?.entries, state?.toolCalls, session?.cwd]);

  useViewAction((a) => {
    if (!activeId) return;
    const cur = useStore.getState().code[activeId];
    if (a === "stop" && cur?.busy) api.codeCancel(activeId);
    if (a === "copy-last") {
      const last = [...(cur?.entries ?? [])].reverse().find((x) => x.kind === "agent");
      if (last && last.kind === "agent") {
        copyText(last.text).then((ok) => ok && setToast("Copied the last reply"));
      }
    }
  });

  // Re-render once foreground tools have been quiet long enough to stop showing Working….
  useEffect(() => {
    if (!state || state.busy || hasActiveForegroundTools(state.toolCalls) || state.lastForegroundAt == null) return;
    const left = FOREGROUND_IDLE_MS - (Date.now() - state.lastForegroundAt);
    if (left <= 0) return;
    const id = window.setTimeout(() => setIdleTick((n) => n + 1), left + 1);
    return () => clearTimeout(id);
  }, [state?.busy, state?.lastForegroundAt, state?.toolCalls]);

  if (!session) {
    return (
      <div className="flex min-w-0 flex-1 flex-col">
        {drag["data-tauri-drag-region"] && (
          <header {...drag} className="flex h-12 shrink-0 items-center justify-end px-4">
            <WindowControls />
          </header>
        )}
      <div className="flex flex-1 flex-col items-center justify-center gap-2.5 px-6 pb-[10vh] text-center text-muted">
        <div className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Code</div>
        <div className="text-[32px] font-medium leading-tight tracking-[-0.02em] text-fg">Point an agent at a folder.</div>
        <div>It reads, edits and runs things there, and asks before anything risky.</div>
        <button
          className="mt-4 flex h-10 items-center gap-2 rounded-[10px] bg-accent px-4 text-sm font-semibold text-accent-fg hover:brightness-105"
          onClick={() => useStore.getState().setNewSessionOpen(true)}
        >
          <PlusIcon size={17} />
          New session…
        </button>
        <AgentPicker />
      </div>
      </div>
    );
  }

  const s = state;
  const status = s?.status ?? "idle";
  const host = remoteHost(session, mesh);
  // The offline banner offers the way forward instead of a restart that can't work.
  const hostOffline = !!host && !memberOnline(mesh, host);
  const ready = status === "ready";
  // Broader than prompt-RPC `s.busy`: Claude (and similar) keep streaming after PromptDone.
  const working = s ? codeWorking(s) : false;
  // Commands the agent backgrounded: its turn is over, but they still run (and it resumes when
  // they end).
  const background = Object.values(s?.toolCalls ?? {}).filter((tc) => tc.background);

  // What the session is waiting on, said plainly. A phone reaches it through a gateway, so it
  // can be reconnecting, or catching up on a session whose agent already runs.
  const reconnecting = remote && !!mesh?.phase && mesh.phase !== "online";
  const waiting = reconnecting
    ? "Reconnecting…"
    : remote && status === "idle"
      ? "Loading session…"
      : status === "starting" && !s?.auth
        ? s?.history
          ? "Resuming session…"
          : "Starting agent…"
        : null;
  const acpModels = s?.models;
  // Only Grok Build runs on Patchbay's models (`pb-<id>`); other agents bring their own.
  const patchbayModels = session.agent === "grok";
  const currentModelRef = patchbayModels
    ? (acpModels?.currentModelId?.replace(/^pb-/, "") ?? session.modelRef)
    : (acpModels?.currentModelId ?? session.agentModel);
  const model = patchbayModels ? models.find((m) => m.id === currentModelRef) : undefined;
  const configOptions = (s?.configOptions ?? []).filter((o) => o.type === "select");

  const setConfigOption = async (o: AcpConfigOption, value: string) => {
    try {
      await api.codeSetConfigOption(session.id, o.id, value);
    } catch (err) {
      setToast(errorText(err));
    }
  };

  const send = async (text: string, atts: Attachment[], mentions: string[] = []) => {
    if (!activeId) return;
    const promptId = crypto.randomUUID();
    // Events still waiting may end the turn: apply them before deciding.
    flushCodeEvents(activeId);
    // Mid-turn the agent steers it in or Patchbay queues it; its `userPrompt` event shows which.
    const midTurn = !!useStore.getState().code[activeId]?.busy;
    if (!midTurn) {
      pushCodeEntry(activeId, { kind: "user", text, attachments: atts, promptId });
      setCodeBusy(activeId, true);
    }
    sticky.pin();
    try {
      const root = session.cwd.replace(/[\\/]+$/, "");
      await api.codePrompt(
        activeId,
        text,
        atts.map((a) => a.id),
        mentions.map((m) => `${root}/${m}`),
        promptId,
      );
      refreshCodeSessions();
    } catch (e) {
      if (!midTurn) setCodeBusy(activeId, false);
      pushCodeEntry(activeId, { kind: "error", text: errorText(e) });
    }
  };

  const editAndResend = async (entryIndex: number, text: string, atts: Attachment[]): Promise<boolean> => {
    if (!activeId || !s) return false;
    const entry = s.entries[entryIndex];
    if (entry?.kind !== "user") return false;
    const ordinal = s.entries.slice(0, entryIndex).filter((x) => x.kind === "user").length;
    const laterTurns = s.entries.slice(entryIndex + 1).filter((x) => x.kind === "user").length;
    const { ok, checked } = await askConfirm({
      title: "Resend edited message?",
      body:
        "Everything in this session after this message will be removed from the agent's history" +
        (laterTurns > 0 ? ` (including ${laterTurns} later prompt${laterTurns === 1 ? "" : "s"})` : "") +
        ". This can't be undone.",
      confirmLabel: "Discard later history and resend",
      danger: true,
      checkbox: { label: "Also revert file changes the agent made after this message", checked: false },
    });
    if (!ok) return false;
    try {
      await api.codeRewind(activeId, ordinal, entry.text, checked);
    } catch (e) {
      setToast(`Could not rewind: ${errorText(e)}`);
      return false;
    }
    truncateCode(activeId, entryIndex);
    await send(text, atts);
    return true;
  };

  editRef.current = editAndResend;

  // The transcript as rows, mounted only near the viewport: earlier agents' part, the cached
  // history, the plan, then this agent's entries.
  const rows: TranscriptRow[] = [];
  if (s?.earlier) {
    const calls = s.earlier.toolCalls;
    rows.push({
      key: "earlier-note",
      dim: true,
      render: () => (
        <div className="text-center text-[12px] text-faint">
          Earlier in this session. The current agent has it only as context, so it can't be edited or rewound.
        </div>
      ),
    });
    for (const r of earlierRuns) {
      rows.push(
        runRow(r, "e", true, (e, i) => (
          <EntryView key={i} e={e} i={i} tc={e.kind === "tool" ? calls[e.id] : undefined} cwd={session.cwd} editing={false} canEdit={false} />
        )),
      );
    }
    rows.push({ key: "earlier-end", dim: true, render: () => <div className="border-t border-line" /> });
  }
  if (s?.history) {
    const calls = s.history.toolCalls;
    if (ready) {
      rows.push({
        key: "history-note",
        dim: true,
        render: () => (
          <div className="text-center text-[12px] text-faint">
            Earlier in this session. The agent started fresh, so this isn't in its context.
          </div>
        ),
      });
    }
    for (const r of historyRuns) {
      rows.push(
        runRow(r, "h", ready, (e, i) => (
          <EntryView key={i} e={e} i={i} tc={e.kind === "tool" ? calls[e.id] : undefined} cwd={session.cwd} editing={false} canEdit={false} />
        )),
      );
    }
    if (ready) rows.push({ key: "history-end", dim: true, render: () => <div className="border-t border-line" /> });
  }
  if (s?.plan && s.plan.length > 0) {
    const plan = s.plan;
    rows.push({ key: "plan", render: () => <PlanPanel plan={plan} /> });
  }
  if (s && (ready || !s.history)) {
    const canEdit = ready && !working && !!s.extensions.rewind;
    for (const r of entryRuns) {
      rows.push(
        runRow(r, "", false, (e, i) => (
          <EntryView
            key={i}
            e={e}
            i={i}
            tc={e.kind === "tool" ? s.toolCalls[e.id] : undefined}
            perm={e.kind === "tool" ? s.permissions.find((p) => p.request.toolCall.toolCallId === e.id) : undefined}
            autoApproved={e.kind === "tool" ? s.autoApproved[e.id] : undefined}
            cwd={session.cwd}
            editing={editingIdx === i}
            canEdit={canEdit && e.kind === "user"}
            onEditing={setEditingIdx}
            onEdit={onEdit}
          />
        )),
      );
    }
  }

  const needsYou = (s?.permissions.length ?? 0) > 0 || !!s?.auth;
  // The status light: amber = needs you, blue = working, green = ready, red = stopped.
  const dot = needsYou
    ? "bg-accent signal-pulse shadow-[0_0_0_3px_rgb(242_165_65/0.18)]"
    : working
      ? "bg-live shadow-[0_0_0_3px_rgb(110_168_254/0.15)]"
      : status === "ready"
        ? "bg-ok shadow-[0_0_0_3px_rgb(91_208_138/0.15)]"
        : status === "starting"
          ? "bg-accent signal-pulse"
          : status === "exited"
            ? "bg-danger"
            : "bg-faint";
  const dotLabel = needsYou ? "needs you" : working ? "working" : status;

  const setAcpMode = async (id: string) => {
    if (!s?.modes) return;
    try {
      await api.codeSetMode(session.id, id);
      useStore.setState((st) => ({
        code: {
          ...st.code,
          [session.id]: { ...st.code[session.id], modes: { ...s.modes!, currentModeId: id } },
        },
      }));
    } catch (err) {
      setToast(errorText(err));
    }
  };
  const setApprovals = async (mode: CodeSession["permissionMode"]) => {
    try {
      await api.codeSetPermissionMode(session.id, mode);
      useStore.setState((st) => ({
        codeSessions: st.codeSessions.map((c) => (c.id === session.id ? { ...c, permissionMode: mode } : c)),
      }));
    } catch (err) {
      setToast(errorText(err));
    }
  };
  const modes = s?.modes?.availableModes ?? [];
  const approval = session.permissionMode || "default";
  // Not "Ask / Auto": Cursor has an agent mode called Ask right next to this control.
  const approvalOpts: { id: CodeSession["permissionMode"]; name: string; title: string }[] = [
    { id: "default", name: "Ask first", title: "Ask before anything the agent asks permission for" },
    {
      id: "auto",
      name: "Auto edits",
      title: "Reads and edits inside the project run without asking; deletes, shell, network and anything outside the project still ask",
    },
    { id: "always-approve", name: "Approve all", title: "Approve everything except plans and questions, without asking" },
  ];
  const gaps = s?.approvalGaps ?? [];

  const agentLabel = session.agent !== "grok" && (
    <span {...drag} className="shrink-0 text-[12px] text-muted" title="Coding agent">
      {AGENT_LABELS[session.agent] ?? session.agent}
    </span>
  );
  // A phone always gets a select: a segmented control doesn't fit beside the others.
  const modeControl =
    s?.modes && modes.length > 0 ? (
      modes.length <= 3 && !remote ? (
        <div
          role="radiogroup"
          aria-label="Agent mode"
          className={`mr-1 flex gap-0.5 rounded-[9px] border border-line bg-panel p-0.5 text-[12px] ${ready ? "" : "pointer-events-none opacity-50"}`}
        >
          {modes.map((m) => {
            const on = m.id === s.modes!.currentModeId;
            return (
              <button
                key={m.id}
                role="radio"
                aria-checked={on}
                title={m.description ?? m.name}
                className={`h-[26px] rounded-[7px] px-2.5 ${on ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"}`}
                onClick={() => !on && setAcpMode(m.id)}
              >
                {m.name}
              </button>
            );
          })}
        </div>
      ) : (
        <ChipSelect
          className={remote ? "shrink-0" : "mr-1 max-w-44"}
          value={s.modes.currentModeId}
          disabled={!ready}
          title="Agent mode"
          aria-label="Agent mode"
          label="Mode"
          onChange={(e) => setAcpMode(e.target.value)}
        >
          {modes.map((m) => (
            <option key={m.id} value={m.id}>
              {m.name}
            </option>
          ))}
        </ChipSelect>
      )
    ) : null;
  const approvalControl = remote ? (
    <ChipSelect
      className={`shrink-0 ${approval === "always-approve" ? "text-danger" : ""}`}
      value={approval}
      title="Approvals"
      aria-label="Approvals"
      onChange={(e) => setApprovals(e.target.value as CodeSession["permissionMode"])}
    >
      {approvalOpts.map((m) => (
        <option key={m.id} value={m.id}>
          {m.name}
        </option>
      ))}
    </ChipSelect>
  ) : (
    <div
      role="radiogroup"
      aria-label="Approvals"
      title="What runs without asking you"
      className="mr-1 flex gap-0.5 rounded-[9px] border border-line bg-panel p-0.5 text-[12px]"
    >
      {approvalOpts.map((m) => {
        const on = m.id === approval;
        const tone = on ? (m.id === "always-approve" ? "bg-panel-2 text-danger" : "bg-panel-2 text-fg") : "text-muted hover:text-fg";
        return (
          <button
            key={m.id}
            role="radio"
            aria-checked={on}
            title={m.title}
            className={`h-[26px] whitespace-nowrap rounded-[7px] px-2.5 ${tone}`}
            onClick={() => !on && setApprovals(m.id)}
          >
            {m.name}
          </button>
        );
      })}
    </div>
  );
  const gapsButton = gaps.length > 0 && (
    <button
      className="mr-1 flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-border font-mono text-[11px] text-muted hover:text-fg"
      title={`Runs without asking, whatever the approval mode:\n${gaps.join("\n")}`}
      aria-label="What this agent runs without asking"
      onClick={() => setToast(gaps.join(" "))}
    >
      i
    </button>
  );

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <header {...drag} className={`flex h-12 shrink-0 items-center gap-1 border-b border-line pr-3 ${leading ? "pl-1" : "pl-6"}`}>
        {leading}
        <span className={`h-2 w-2 shrink-0 rounded-full ${dot}`} title={dotLabel} role="img" aria-label={`Agent ${dotLabel}`} />
        <div {...drag} className="mx-2.5 flex min-w-0 flex-1 items-baseline gap-2.5">
          <h1 {...drag} className="shrink truncate text-[15px] font-medium" style={{ minWidth: "4rem" }}>
            {session.title}
          </h1>
          {!remote && (
            <>
              <span {...drag} className="min-w-0 flex-[1_1_0] truncate font-mono text-[12px] text-faint" title={session.cwd}>
                {session.cwd.replace(/^\/home\/[^/]+|^\/Users\/[^/]+/, "~")}
              </span>
              {agentLabel}
              <HostBadge session={session} />
            </>
          )}
        </div>
        {!remote && (
          <>
            {modeControl}
            {approvalControl}
            {gapsButton}
          </>
        )}
        <div className="mx-1 h-5 w-px bg-line" />
        {!remote && (
          <ExportMenu
            disabled={!s || s.entries.length === 0}
            onExport={(f) =>
              s &&
              saveExport(
                session.title,
                f,
                f === "markdown" ? codeToMarkdown(session, s.entries, s.toolCalls) : codeToJson(session, s.entries, s.toolCalls),
              )
            }
          />
        )}
        <HeaderButton
          label="Changes"
          active={showChanges}
          badge={changes.length}
          onClick={() => setShowChanges(!showChanges)}
        >
          <FileDiffIcon />
        </HeaderButton>
        {!remote && mesh?.member && mesh.members.filter((m) => m.state !== "removed").length > 1 && (
          <HeaderButton label="Move to another computer" disabled={working} onClick={() => setMoving("move")}>
            <MonitorUpIcon />
          </HeaderButton>
        )}
        {!remote && (
          <HeaderButton
            label={host ? `Terminal on ${memberName(mesh, host)}` : "Terminal"}
            active={showTerm}
            disabled={hostOffline}
            onClick={() => {
              setShowTerm(!showTerm);
              setTermMounted((m) => ({ ...m, [session.id]: true }));
            }}
          >
            <TerminalIcon />
          </HeaderButton>
        )}
        <HeaderButton label="Restart the agent and reload this session" onClick={() => start(session.id)}>
          <RestartIcon />
        </HeaderButton>
        <WindowControls />
      </header>

      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="relative min-h-0 flex-1">
            <ScrollFade show={!sticky.atTop} />
            <div
              ref={sticky.ref}
              className={`relative h-full overflow-y-auto [overflow-anchor:none] ${sticky.atTop ? "" : "fade-top"}`}
              onScroll={sticky.onScroll}
            >
              <div className="mx-auto flex max-w-3xl flex-col gap-4 px-6 py-6">
                <OfflineHostBanner session={session} onTakeOver={() => setMoving("takeOver")} />
                {waiting && (s?.entries.length ?? 0) === 0 && (
                  <div className="flex items-center justify-center gap-2 text-xs text-muted">
                    <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-accent" />
                    {waiting}
                  </div>
                )}
                {ready && (s?.entries.length ?? 0) === 0 && !s?.history && !s?.earlier && (
                  <div className="mt-16 text-center text-muted">
                    Ask the agent to read, change, or run things in{" "}
                    <span className="font-mono text-[13px] text-fg/80">{session.cwd}</span>.
                  </div>
                )}
                {rows.length > 0 && <TranscriptRows rows={rows} scroller={sticky.el} />}
                {s?.permissions
                  .filter((p) => !s.entries.some((e) => e.kind === "tool" && e.id === p.request.toolCall.toolCallId))
                  .map((p) => (
                  <PermissionCard
                    key={p.requestId}
                    requestId={p.requestId}
                    request={p.request}
                    live={s.toolCalls[p.request.toolCall.toolCallId]}
                    cwd={session.cwd}
                  />
                ))}
                {s?.auth && (
                  <SignInCard
                    key={session.id}
                    sessionId={session.id}
                    cwd={session.cwd}
                    message={s.auth.message}
                    methods={s.auth.methods}
                    again={status === "ready"}
                  />
                )}
                {working && s?.permissions.length === 0 && (
                  <div className="flex items-center gap-2 text-xs text-muted">
                    <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-live" /> Working…
                  </div>
                )}
                {background.length > 0 && (
                  <div className="flex items-center gap-2 text-xs text-muted" title={background.map(backgroundLabel).join("\n")}>
                    <span className="signal-pulse h-1.5 w-1.5 shrink-0 rounded-full bg-live" />
                    <span className="min-w-0 truncate">
                      {background.length === 1
                        ? `Running in the background: ${backgroundLabel(background[0])}`
                        : `${background.length} commands running in the background`}
                    </span>
                  </div>
                )}
                {status === "exited" && !hostOffline && (
                  <div className="flex items-center justify-center gap-2 text-xs text-muted">
                    Agent stopped.
                    <button className="text-accent-ink underline underline-offset-2" onClick={() => start(session.id)}>
                      Restart
                    </button>
                  </div>
                )}
              </div>
            </div>
            <JumpToBottom show={!sticky.atBottom} onClick={sticky.toBottom} />
          </div>
          <div className="mx-auto w-full max-w-3xl px-6 pb-3 pt-2">
            {(s?.queued.length ?? 0) > 0 && (
              <ul className="mb-2 flex flex-col gap-1" aria-label="Queued messages">
                {s!.queued.map((q) => (
                  <li
                    key={q.promptId}
                    className="flex items-center gap-2 rounded-[10px] border border-border bg-panel px-3 py-1.5 text-xs"
                  >
                    <span className="shrink-0 font-mono text-[10px] uppercase tracking-[0.08em] text-faint">Queued</span>
                    <span className="min-w-0 flex-1 truncate text-muted">
                      {q.text.trim() || q.attachments?.map((a) => a.name).join(", ")}
                    </span>
                    <button
                      className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
                      onClick={() => api.codeUnqueue(session.id, q.promptId).catch((e) => setToast(errorText(e)))}
                      title="Don't send"
                      aria-label="Remove queued message"
                    >
                      <XIcon size={12} />
                    </button>
                  </li>
                ))}
              </ul>
            )}
            <Composer
              busy={working}
              // Hosts that steer or queue say whether the agent steers; an older host would send a
              // second prompt into the running turn.
              sendWhileBusy={s?.extensions.steering !== undefined}
              busySendHint={
                s?.extensions.steering
                  ? "Send now: the agent reads it during this turn"
                  : "Queue: sends when the agent finishes this turn"
              }
              restore={s?.restore}
              onRestored={() => clearCodeRestore(session.id)}
              disabled={!ready}
              commands={s?.commands}
              mentionFiles={() => api.listProjectFiles(session.cwd, session.id)}
              onEditLast={() => {
                if (!ready || working || !s) return;
                for (let i = s.entries.length - 1; i >= 0; i--) {
                  if (s.entries[i].kind === "user") return setEditingIdx(i);
                }
              }}
              onStop={() => api.codeCancel(session.id)}
              onSend={send}
              placeholder={
                reconnecting
                  ? "Reconnecting…"
                  : ready && working
                    ? s?.extensions.steering
                      ? "Add to what the agent is doing…"
                      : "Queue a message for when the agent finishes…"
                    : ready
                    ? `Ask the agent…${s?.commands?.length ? "  / for commands, @ for files" : "  @ for files"}`
                    : (waiting ?? (s?.auth ? "Sign in to start the agent" : "Agent not running"))
              }
              warnNoVision={model ? !model.vision : false}
              footerLabel="Session settings"
              trailing={remote && <HostBadge session={session} />}
              footer={
                <>
                <SessionAgentPicker
                  session={session}
                  host={host}
                  started={[s?.earlier, s?.history, s].some((p) => (p?.entries ?? []).some((x) => x.kind === "user"))}
                  // Waiting for a sign-in, the user may pick another agent instead.
                  disabled={working || (status === "starting" && !s?.auth) || hostOffline || !!reconnecting}
                />
                {acpModels && acpModels.availableModels.length > 0 ? (
                  <ChipSelect
                    lead={<ModelRing id={currentModelRef} />}
                    mono
                    className="max-w-60"
                    value={acpModels.currentModelId}
                    disabled={!ready || working}
                    aria-label="Agent model"
                    label="Model"
                    onChange={async (e) => {
                      const id = e.target.value;
                      try {
                        await api.codeSetModel(session.id, id);
                        useStore.setState((st) => ({
                          code: {
                            ...st.code,
                            [session.id]: { ...st.code[session.id], models: { ...acpModels, currentModelId: id } },
                          },
                        }));
                        refreshCodeSessions();
                      } catch (err) {
                        setToast(errorText(err));
                      }
                    }}
                  >
                    {acpModels.availableModels.map((m) => (
                      <option key={m.modelId} value={m.modelId}>
                        {m.name}
                      </option>
                    ))}
                  </ChipSelect>
                ) : (
                  model && (
                    <ChipText label="Model">
                      <ModelRing id={model.id} />
                      <span className="truncate">{model.displayName}</span>
                    </ChipText>
                  )
                )}
                {configOptions.map((o) => (
                  <ChipSelect
                    key={o.id}
                    className="max-w-40"
                    value={o.currentValue}
                    disabled={!ready || working}
                    title={o.description ?? o.name}
                    aria-label={o.name}
                    onChange={(e) => setConfigOption(o, e.target.value)}
                  >
                    {configChoices(o).map((c) => (
                      <option key={c.value} value={c.value}>
                        {c.name}
                      </option>
                    ))}
                  </ChipSelect>
                ))}
                {remote && (
                  <>
                    {modeControl}
                    <div>
                      {approvalControl}
                      {gaps.length > 0 && (
                        <div className="px-3 pb-3 text-[12.5px] leading-5 text-muted">
                          Runs without asking, whatever the approval mode: {gaps.join(" ")}
                        </div>
                      )}
                    </div>
                  </>
                )}
                </>
              }
            />
            <UsageBar
              totals={s?.usage?.totals ?? null}
              contextTokens={s?.usage?.contextTokens ?? null}
              contextWindow={s?.usage?.contextWindow ?? model?.contextWindow ?? null}
              autoCompactPercent={s?.usage?.autoCompactPercent ?? null}
              estimatedCostUsd={s?.usage ? estimateCost(s.usage.totals, model?.params.pricing) : null}
              onConfigureWindow={() => useStore.getState().setSettingsOpen(true)}
            />
          </div>
        </div>
        {showChanges && (
          <aside className="w-[42%] min-w-80 shrink-0 border-l border-line bg-panel/50">
            <ChangesPanel changes={changes} cwd={session.cwd} onClose={() => setShowChanges(false)} />
          </aside>
        )}
      </div>

      {termMounted[session.id] && (
        <div className={`h-64 shrink-0 border-t border-line ${showTerm ? "" : "hidden"}`}>
          {/* Keyed by host, so a moved session's terminal reopens on its new computer. */}
          <TerminalPane key={session.hostNode ?? ""} id={session.id} cwd={session.cwd} visible={showTerm} />
        </div>
      )}
      {moving && <MoveDialog session={session} takeOver={moving === "takeOver"} onClose={() => setMoving(null)} />}
    </div>
  );
}
