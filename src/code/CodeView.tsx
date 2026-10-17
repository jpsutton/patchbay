import { useEffect, useMemo, useState, type ReactNode } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  api,
  errorText,
  estimateCost,
  type AcpToolCall,
  type AcpToolCallContent,
  type Attachment,
} from "../lib/ipc";
import { memberName, memberOnline, remoteHost, useStore, type CodeState, type Entry } from "../lib/store";
import { HostBadge, MoveDialog, OfflineHostBanner } from "./HostControls";
import { Composer } from "../components/Composer";
import { Markdown } from "../components/Markdown";
import { EditMessage } from "../components/EditMessage";
import { UsageBar } from "../components/UsageBar";
import { TerminalPane } from "../terminal/TerminalPane";
import { PermissionCard, ToolCallCard, ToolGroup } from "./ToolCallCard";
import { DiffView, diffStats } from "./DiffView";
import { CopyButton, copyText } from "../components/CopyButton";
import { ExportMenu } from "../components/ExportMenu";
import { codeToJson, codeToMarkdown, saveExport } from "../lib/export";
import { useViewAction } from "../lib/shortcuts";
import { HeaderButton } from "../components/HeaderButton";
import { useDragRegion, WindowControls } from "../components/WindowControls";
import {
  CheckIcon,
  ChevronRightIcon,
  CopyIcon,
  FileDiffIcon,
  FolderPlusIcon,
  MonitorUpIcon,
  PaperclipIcon,
  PencilIcon,
  RestartIcon,
  TerminalIcon,
  XIcon,
} from "../components/icons";
import { ChipSelect } from "../components/ChipSelect";
import { ModelRing } from "../lib/cable";
import { JumpToBottom, ScrollFade, useStickyScroll } from "../lib/useStickyScroll";

export const NEW_SESSION_TITLE = "New session";

export async function newSessionIn(dir: string) {
  const st = useStore.getState();
  try {
    const s = await api.createCodeSession(dir, st.defaultModelRef);
    await st.refreshCodeSessions();
    st.setMode("code");
    st.selectCodeSession(s.id);
  } catch (e) {
    st.setToast(errorText(e));
  }
}

export async function openFolderAsSession() {
  const dir = await open({ directory: true, multiple: false, title: "Open project folder" });
  if (!dir || Array.isArray(dir)) return;
  await newSessionIn(dir);
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

function EntryView({
  e,
  s,
  cwd,
  editing,
  setEditing,
  onEdit,
}: {
  e: Entry;
  s: Pick<CodeState, "toolCalls" | "permissions" | "autoApproved">;
  cwd: string;
  editing: boolean;
  setEditing: (on: boolean) => void;
  onEdit?: (text: string, atts: Attachment[]) => Promise<boolean>;
}) {
  switch (e.kind) {
    case "user":
      return <UserEntry e={e} editing={editing} setEditing={setEditing} onEdit={onEdit} />;
    case "agent":
      return <AgentEntry text={e.text} />;
    case "thought":
      return <Thought text={e.text} />;
    case "tool": {
      const tc = s.toolCalls[e.id];
      const perm = s.permissions.find((p) => p.request.toolCall.toolCallId === e.id);
      if (perm) return <PermissionCard requestId={perm.requestId} request={perm.request} live={tc} cwd={cwd} />;
      return tc ? <ToolCallCard tc={tc} autoApproved={s.autoApproved[e.id]} /> : null;
    }
    case "error":
      return (
        <div className="whitespace-pre-wrap rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 font-mono text-xs text-danger">
          {e.text}
        </div>
      );
    case "info":
      return <div className="text-center text-[12px] text-faint">{e.text}</div>;
  }
}

function PlanPanel({ plan }: { plan: NonNullable<CodeState["plan"]> }) {
  if (plan.length === 0) return null;
  const done = plan.filter((p) => p.status === "completed").length;
  return (
    <section aria-label="Plan" className="flex flex-col gap-2 rounded-xl border border-line bg-panel/60 px-3.5 py-3 text-[13px]">
      <div className="flex items-center font-mono text-[11px] text-faint">
        <span className="uppercase tracking-[0.06em]">Plan</span>
        <span className="flex-1" />
        <span>
          {done} of {plan.length}
        </span>
      </div>
      <div
        className="h-[3px] overflow-hidden rounded-full bg-panel-2"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={plan.length}
        aria-valuenow={done}
        aria-label="Plan progress"
      >
        <div className="h-full bg-accent transition-[width]" style={{ width: `${(done / plan.length) * 100}%` }} />
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
              ) : (
                <span className="h-2 w-2 rounded-full border-[1.5px] border-faint/60" />
              )}
            </span>
            <span
              className={
                p.status === "completed"
                  ? "text-muted line-through decoration-faint/40"
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

type Run = { i: number; e: Entry } | { tools: { i: number; e: Entry }[] };

/**
 * Consecutive tool calls form one run, drawn as a single box with hairlines between rows. A call
 * waiting on a permission stays on its own, so its card can't hide inside a group.
 */
function runsOf(entries: Entry[], s: Pick<CodeState, "toolCalls" | "permissions">): Run[] {
  const out: Run[] = [];
  entries.forEach((e, i) => {
    const groupable =
      e.kind === "tool" && !!s.toolCalls[e.id] && !s.permissions.some((p) => p.request.toolCall.toolCallId === e.id);
    const last = out[out.length - 1];
    if (groupable && last && "tools" in last) last.tools.push({ i, e });
    else if (groupable) out.push({ tools: [{ i, e }] });
    else out.push({ i, e });
  });
  return out;
}

function renderRuns(runs: Run[], one: (e: Entry, i: number) => ReactNode): ReactNode[] {
  return runs.map((r) =>
    "tools" in r ? (
      <ToolGroup key={`g${r.tools[0].i}`}>{r.tools.map(({ e, i }) => one(e, i))}</ToolGroup>
    ) : (
      one(r.e, r.i)
    ),
  );
}

type Diff = Extract<AcpToolCallContent, { type: "diff" }>;

/** Collapse every diff in the session to one per path: first oldText → last newText. */
function collectChanges(order: Entry[], toolCalls: Record<string, AcpToolCall>): Diff[] {
  const byPath = new Map<string, Diff>();
  for (const e of order) {
    if (e.kind !== "tool") continue;
    const tc = toolCalls[e.id];
    if (!tc || tc.status === "failed") continue;
    for (const c of tc.content ?? []) {
      if (c.type !== "diff") continue;
      const prev = byPath.get(c.path);
      byPath.set(c.path, prev ? { ...c, oldText: prev.oldText } : c);
    }
  }
  return [...byPath.values()];
}

function ChangesPanel({ changes, cwd, onClose }: { changes: Diff[]; cwd: string; onClose: () => void }) {
  const [sel, setSel] = useState<string | null>(null);
  const cur = changes.find((c) => c.path === sel) ?? changes[0];
  const rel = (p: string) => (p.startsWith(cwd + "/") ? p.slice(cwd.length + 1) : p);
  const stats = changes.map((c) => diffStats(c.oldText, c.newText));
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
          const isNew = c.oldText === null || c.oldText === "";
          return (
            <li key={c.path}>
            <button
              className={`flex h-8 w-full items-center gap-2 rounded-md px-2.5 text-left hover:bg-panel-2 ${
                cur?.path === c.path ? "bg-panel-2 text-fg" : "text-fg/75"
              }`}
              onClick={() => setSel(c.path)}
            >
              <span className={`w-3 font-mono text-[11px] ${isNew ? "text-ok" : "text-accent-ink"}`} title={isNew ? "Added" : "Modified"}>
                {isNew ? "A" : "M"}
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
      <div className="min-h-0 flex-1 overflow-auto p-2">
        {cur && <DiffView path={rel(cur.path)} oldText={cur.oldText} newText={cur.newText} maxHeight={10000} />}
      </div>
    </div>
  );
}

export function CodeView() {
  const drag = useDragRegion();
  const activeId = useStore((s) => s.activeCodeSessionId);
  const sessions = useStore((s) => s.codeSessions);
  const state = useStore((s) => (activeId ? s.code[activeId] : undefined));
  const models = useStore((s) => s.models);
  const pushCodeEntry = useStore((s) => s.pushCodeEntry);
  const setCodeBusy = useStore((s) => s.setCodeBusy);
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
  const sticky = useStickyScroll([state?.entries, state?.toolCalls, state?.permissions]);
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
    if (sess?.hostNode && !meshLoaded) return;
    // Running on another computer: watch it there, never restart it just because we looked.
    if (remoteHost(sess, useStore.getState().mesh)) {
      attach(activeId);
      return;
    }
    api.codeIsRunning(activeId).then((running) => {
      // A running agent without UI state (e.g. after a reload) is restarted so history replays.
      if (!running || !cur) start(activeId);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId, meshLoaded]);

  const changes = useMemo(
    () => (state ? collectChanges(state.entries, state.toolCalls) : []),
    [state?.entries, state?.toolCalls],
  );

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
          onClick={openFolderAsSession}
        >
          <FolderPlusIcon size={17} />
          Open folder…
        </button>
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
  const busy = !!s?.busy;
  const acpModels = s?.models;
  const currentModelRef = acpModels?.currentModelId?.replace(/^pb-/, "") ?? session.modelRef;
  const model = models.find((m) => m.id === currentModelRef);

  const send = async (text: string, atts: Attachment[], mentions: string[] = []) => {
    if (!activeId) return;
    const isFirst = !(useStore.getState().code[activeId]?.entries ?? []).some((x) => x.kind === "user");
    const folder = session.cwd.split(/[\\/]/).filter(Boolean).pop();
    if (isFirst && (session.title === NEW_SESSION_TITLE || session.title === folder)) {
      const line = text.split("\n").find((l) => l.trim())?.trim() ?? atts[0]?.name ?? "";
      if (line) {
        api.updateCodeSession(session.id, { title: line.length > 60 ? `${line.slice(0, 60)}…` : line })
          .then(refreshCodeSessions);
      }
    }
    pushCodeEntry(activeId, { kind: "user", text, attachments: atts });
    setCodeBusy(activeId, true);
    sticky.pin();
    try {
      const root = session.cwd.replace(/[\\/]+$/, "");
      await api.codePrompt(
        activeId,
        text,
        atts.map((a) => a.id),
        mentions.map((m) => `${root}/${m}`),
      );
      refreshCodeSessions();
    } catch (e) {
      setCodeBusy(activeId, false);
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

  const needsYou = (s?.permissions.length ?? 0) > 0;
  // The status light: amber = needs you, blue = working, green = ready, red = stopped.
  const dot = needsYou
    ? "bg-accent signal-pulse shadow-[0_0_0_3px_rgb(242_165_65/0.18)]"
    : busy
      ? "bg-live shadow-[0_0_0_3px_rgb(110_168_254/0.15)]"
      : status === "ready"
        ? "bg-ok shadow-[0_0_0_3px_rgb(91_208_138/0.15)]"
        : status === "starting"
          ? "bg-accent signal-pulse"
          : status === "exited"
            ? "bg-danger"
            : "bg-faint";
  const dotLabel = needsYou ? "needs you" : busy ? "working" : status;

  const setPermissionMode = async (id: string) => {
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
  const modes = s?.modes?.availableModes ?? [];

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <header {...drag} className="flex h-12 shrink-0 items-center gap-1 border-b border-line pl-6 pr-3">
        <span className={`h-2 w-2 shrink-0 rounded-full ${dot}`} title={dotLabel} role="img" aria-label={`Agent ${dotLabel}`} />
        <div {...drag} className="mx-2.5 flex min-w-0 flex-1 items-baseline gap-2.5">
          <h1 {...drag} className="shrink truncate text-[15px] font-medium" style={{ minWidth: "4rem" }}>
            {session.title}
          </h1>
          <span {...drag} className="min-w-0 flex-[1_1_0] truncate font-mono text-[12px] text-faint" title={session.cwd}>
            {session.cwd.replace(/^\/home\/[^/]+|^\/Users\/[^/]+/, "~")}
          </span>
          <HostBadge session={session} />
        </div>
        {modes.length > 0 && modes.length <= 3 && s?.modes && (
          <div
            role="radiogroup"
            aria-label="Permission mode"
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
                  onClick={() => !on && setPermissionMode(m.id)}
                >
                  {m.name}
                </button>
              );
            })}
          </div>
        )}
        {modes.length > 3 && s?.modes && (
          <ChipSelect
            className="mr-1 max-w-44"
            value={s.modes.currentModeId}
            disabled={!ready}
            title="Permission mode"
            aria-label="Permission mode"
            onChange={(e) => setPermissionMode(e.target.value)}
          >
            {modes.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </ChipSelect>
        )}
        <div className="mx-1 h-5 w-px bg-line" />
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
        <HeaderButton
          label="Changes"
          active={showChanges}
          badge={changes.length}
          onClick={() => setShowChanges(!showChanges)}
        >
          <FileDiffIcon />
        </HeaderButton>
        {mesh?.member && mesh.members.filter((m) => m.state !== "removed").length > 1 && (
          <HeaderButton label="Move to another computer" disabled={busy} onClick={() => setMoving("move")}>
            <MonitorUpIcon />
          </HeaderButton>
        )}
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
              className={`h-full overflow-y-auto [overflow-anchor:none] ${sticky.atTop ? "" : "fade-top"}`}
              onScroll={sticky.onScroll}
            >
              <div className="mx-auto flex max-w-3xl flex-col gap-4 px-6 py-6">
                <OfflineHostBanner session={session} onTakeOver={() => setMoving("takeOver")} />
                {status === "starting" && (s?.entries.length ?? 0) === 0 && (
                  <div className="flex items-center justify-center gap-2 text-xs text-muted">
                    <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-accent" />
                    {s?.history ? "Resuming session…" : "Starting agent…"}
                  </div>
                )}
                {s?.history && (
                  <div className={ready ? "flex flex-col gap-4 opacity-70" : "flex flex-col gap-4"}>
                    {ready && (
                      <div className="text-center text-[12px] text-faint">
                        Earlier in this session. The agent started fresh, so this isn't in its context.
                      </div>
                    )}
                    {renderRuns(runsOf(s.history.entries, { toolCalls: s.history.toolCalls, permissions: [] }), (e, i) => (
                      <EntryView
                        key={`h${i}`}
                        e={e}
                        s={{ toolCalls: s.history!.toolCalls, permissions: [], autoApproved: {} }}
                        cwd={session.cwd}
                        editing={false}
                        setEditing={() => {}}
                      />
                    ))}
                    {ready && <div className="border-t border-line" />}
                  </div>
                )}
                {ready && (s?.entries.length ?? 0) === 0 && !s?.history && (
                  <div className="mt-16 text-center text-muted">
                    Ask the agent to read, change, or run things in{" "}
                    <span className="font-mono text-[13px] text-fg/80">{session.cwd}</span>.
                  </div>
                )}
                {s?.plan && <PlanPanel plan={s.plan} />}
                {(ready || !s?.history) && s && renderRuns(runsOf(s.entries, s), (e, i) => (
                  <EntryView
                    key={i}
                    e={e}
                    s={s}
                    cwd={session.cwd}
                    editing={editingIdx === i}
                    setEditing={(on) => setEditingIdx(on ? i : null)}
                    onEdit={ready && !busy && e.kind === "user" ? (t, a) => editAndResend(i, t, a) : undefined}
                  />
                ))}
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
                {busy && s?.permissions.length === 0 && (
                  <div className="flex items-center gap-2 text-xs text-muted">
                    <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-live" /> Working…
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
            <Composer
              busy={busy}
              disabled={!ready}
              commands={s?.commands}
              mentionFiles={() => api.listProjectFiles(session.cwd, session.id)}
              onEditLast={() => {
                if (!ready || busy || !s) return;
                for (let i = s.entries.length - 1; i >= 0; i--) {
                  if (s.entries[i].kind === "user") return setEditingIdx(i);
                }
              }}
              onStop={() => api.codeCancel(session.id)}
              onSend={send}
              placeholder={
                ready
                  ? `Ask the agent…${s?.commands?.length ? "  / for commands, @ for files" : "  @ for files"}`
                  : status === "starting"
                    ? "Starting agent…"
                    : "Agent not running"
              }
              warnNoVision={model ? !model.vision : false}
              footer={
                acpModels && acpModels.availableModels.length > 0 ? (
                  <ChipSelect
                    lead={<ModelRing id={currentModelRef} />}
                    mono
                    className="max-w-60"
                    value={acpModels.currentModelId}
                    disabled={!ready || busy}
                    aria-label="Agent model"
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
                    <span className="flex min-w-0 items-center gap-2 px-1 font-mono text-[12px] text-muted">
                      <ModelRing id={model.id} />
                      <span className="truncate">{model.displayName}</span>
                    </span>
                  )
                )
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
