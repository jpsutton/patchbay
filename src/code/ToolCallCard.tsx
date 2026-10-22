import { useState, type ReactNode } from "react";
import type { AcpPermissionRequest, AcpToolCall, AcpToolCallContent } from "../lib/ipc";
import { api, errorText } from "../lib/ipc";
import { mergeToolCall, useStore } from "../lib/store";
import { DiffView, diffStats } from "./DiffView";
import { Markdown } from "../components/Markdown";
import { CopyButton } from "../components/CopyButton";
import { AgentImage } from "../components/AgentImage";
import {
  ArrowRightIcon,
  BrainIcon,
  CheckIcon,
  ChevronRightIcon,
  CopyIcon,
  DotIcon,
  FileTextIcon,
  GlobeIcon,
  MoveIcon,
  PencilIcon,
  SearchIcon,
  TerminalIcon,
  TrashIcon,
  XIcon,
} from "../components/icons";

function StatusMark({ status, background }: { status: string; background?: boolean }) {
  if (background) {
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-live" title="The command runs on in the background">
        <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-live" />
        background
      </span>
    );
  }
  switch (status) {
    case "completed":
      return (
        <span className="flex text-ok" title="Done" aria-label="Done" role="img">
          <CheckIcon size={14} strokeWidth={2.4} />
        </span>
      );
    case "failed":
      return (
        <span className="flex text-danger" title="Failed" aria-label="Failed" role="img">
          <XIcon size={14} strokeWidth={2.4} />
        </span>
      );
    case "in_progress":
      return (
        <span className="flex items-center gap-1.5 text-[12px] text-live">
          <span className="signal-pulse h-1.5 w-1.5 rounded-full bg-live" />
          running
        </span>
      );
    default:
      return <span className="text-[12px] text-faint">pending</span>;
  }
}

const KIND_ICON: Record<string, typeof FileTextIcon> = {
  read: FileTextIcon,
  edit: PencilIcon,
  delete: TrashIcon,
  move: MoveIcon,
  search: SearchIcon,
  execute: TerminalIcon,
  think: BrainIcon,
  fetch: GlobeIcon,
  switch_mode: ArrowRightIcon,
  other: DotIcon,
};

function textOfContent(c: AcpToolCallContent): string | null {
  if (c.type === "content" && c.content.type === "text") return c.content.text;
  return null;
}

type ImageBlock = { data: string; mimeType: string };

/** Images in a tool's output, e.g. a screenshot an agent read back to look at. */
function imagesOf(tc: AcpToolCall): ImageBlock[] {
  return (tc.content ?? []).flatMap((c) =>
    c.type === "content" && c.content.type === "image" && c.content.data ? [c.content] : [],
  );
}

/** A tool's images, each opening full size. */
function ToolImages({ images, label }: { images: ImageBlock[]; label: string }) {
  return (
    <div className="flex flex-col gap-2">
      {images.map((im, i) => (
        <AgentImage key={i} data={im.data} mimeType={im.mimeType} alt={label} />
      ))}
    </div>
  );
}

function inputSummary(raw: unknown): string | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  for (const k of ["command", "cmd", "path", "file_path", "target_file", "pattern", "query", "url"]) {
    if (typeof r[k] === "string") return r[k] as string;
  }
  return null;
}

/** Readable text from a tool's raw output: a string, or the usual stdout/stderr/output fields. */
function rawOutputText(raw: unknown): string | null {
  if (raw == null) return null;
  if (typeof raw === "string") return raw || null;
  if (typeof raw !== "object") return String(raw);
  const r = raw as Record<string, unknown>;
  const parts = ["output", "stdout", "stderr", "result", "content"]
    .map((k) => r[k])
    .filter((v): v is string => typeof v === "string" && v.length > 0);
  if (parts.length) return parts.join("\n");
  const json = JSON.stringify(raw, null, 2);
  return json === "{}" ? null : json;
}

/** A shell command in full, selectable and with a copy button: the row above truncates it. */
function CommandBlock({ cmd }: { cmd: string }) {
  return (
    <div className="flex items-start gap-1 rounded-lg border border-line bg-bg py-1 pl-3 pr-1">
      <pre className="min-w-0 flex-1 overflow-x-auto whitespace-pre-wrap break-words py-1.5 font-mono text-[12.5px] leading-5 text-fg">
        <span className="select-none text-faint">$ </span>
        {cmd}
      </pre>
      <span className="shrink-0 text-faint">
        <CopyButton text={cmd} label="Copy command" icon={<CopyIcon size={14} />} />
      </span>
    </div>
  );
}

export function ToolCallBody({ tc, withImages = true }: { tc: AcpToolCall; withImages?: boolean }) {
  const content = tc.content ?? [];
  const cmd = tc.kind === "execute" ? commandOf(tc) : null;
  const images = imagesOf(tc);
  const diffs = content.filter((c): c is Extract<AcpToolCallContent, { type: "diff" }> => c.type === "diff");
  const terminals = content.filter((c): c is Extract<AcpToolCallContent, { type: "terminal" }> => c.type === "terminal");
  const texts = content.map(textOfContent).filter((t): t is string => !!t);
  // Patchbay doesn't host ACP terminals, so their output only reaches us as raw tool output.
  // A tool that returned an image has it in rawOutput too, as base64: never show that as text.
  const output = texts.length === 0 && diffs.length === 0 && images.length === 0 && !tc.terminalOutput ? rawOutputText(tc.rawOutput) : null;
  return (
    <div className="flex flex-col gap-2">
      {cmd && <CommandBlock cmd={cmd} />}
      {withImages && images.length > 0 && <ToolImages images={images} label={tc.title || "Tool output"} />}
      {diffs.map((d, i) => (
        <DiffView key={i} path={d.path} oldText={d.oldText} newText={d.newText} />
      ))}
      {terminals.length > 0 && !tc.terminalOutput && (
        <div className="flex items-center gap-2 text-[12px] text-muted">
          <TerminalIcon size={13} />
          <span>
            {tc.background
              ? "Running in the background; the agent picks up its output when it ends."
              : tc.status === "in_progress"
                ? "Running…"
                : "No output."}
          </span>
        </div>
      )}
      {tc.terminalOutput && (
        <pre className="max-h-72 overflow-auto whitespace-pre-wrap rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12.5px] leading-5">
          {tc.terminalOutput.trimEnd()}
        </pre>
      )}
      {tc.terminalExit && tc.terminalExit.exitCode !== null && tc.terminalExit.exitCode !== 0 && (
        <div className="font-mono text-[12px] text-danger">exit code {tc.terminalExit.exitCode}</div>
      )}
      {(texts.length > 0 || output) && (
        <pre className="max-h-72 overflow-auto whitespace-pre-wrap rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12.5px] leading-5">
          {texts.length > 0 ? texts.join("\n") : output}
        </pre>
      )}
      {!cmd && diffs.length === 0 && texts.length === 0 && images.length === 0 && terminals.length === 0 && !output && tc.rawInput != null && (
        <pre className="max-h-48 overflow-auto whitespace-pre-wrap rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12.5px] leading-5 text-muted">
          {JSON.stringify(tc.rawInput, null, 2)}
        </pre>
      )}
    </div>
  );
}

/** One tool call as a row; consecutive rows sit together in a `ToolGroup` box. */
export function ToolCallCard({ tc, autoApproved }: { tc: AcpToolCall; autoApproved?: string }) {
  const hasDiff = (tc.content ?? []).some((c) => c.type === "diff");
  const [open, setOpen] = useState(hasDiff);
  const summary = inputSummary(tc.rawInput);
  const Icon = KIND_ICON[tc.kind ?? "other"] ?? DotIcon;
  // Images show without opening the row: they're usually what the agent wanted you to see.
  const images = imagesOf(tc);
  const stats = (tc.content ?? [])
    .filter((c): c is Extract<AcpToolCallContent, { type: "diff" }> => c.type === "diff")
    .map((d) => diffStats(d.oldText, d.newText))
    .reduce((a, b) => ({ add: a.add + b.add, del: a.del + b.del }), { add: 0, del: 0 });
  return (
    <div>
      <button
        className="flex h-[38px] w-full items-center gap-2.5 px-3 text-left text-[13px] hover:bg-panel-2/40"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
      >
        <span className="flex shrink-0 text-faint">
          <Icon size={15} />
        </span>
        <span className="min-w-0 flex-1 truncate">
          <span className="text-fg/85">{tc.title || tc.kind || "Tool call"}</span>
          {summary && tc.title !== summary && (
            <span className="ml-2 font-mono text-[12px] text-faint" title={summary}>
              {summary}
            </span>
          )}
        </span>
        {(stats.add > 0 || stats.del > 0) && (
          <span className="meta shrink-0">
            <span className="text-ok">+{stats.add}</span> <span className="text-danger">−{stats.del}</span>
          </span>
        )}
        {autoApproved && (
          <span className="shrink-0 rounded-[5px] border border-border px-1.5 text-[11px] text-faint" title={autoApproved}>
            auto
          </span>
        )}
        <StatusMark status={tc.status ?? "pending"} background={tc.background} />
        <span className={`flex shrink-0 text-faint transition-transform ${open ? "rotate-90" : ""}`}>
          <ChevronRightIcon size={12} />
        </span>
      </button>
      {images.length > 0 && (
        <div className="border-t border-line px-3 py-2.5">
          <ToolImages images={images} label={tc.title || "Tool output"} />
        </div>
      )}
      {open && (
        <div className="border-t border-line px-3 py-2.5">
          <ToolCallBody tc={tc} withImages={false} />
        </div>
      )}
    </div>
  );
}

/**
 * A plan document an agent wrote as a tool call (Cursor's `createPlan`, Claude Code's
 * `ExitPlanMode`): the Markdown sits in the raw input, while the tool's own output only says
 * where it was saved.
 */
export function planDocOf(tc: AcpToolCall): { name: string | null; text: string } | null {
  const raw = tc.rawInput;
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  if (typeof r.plan !== "string" || !r.plan.trim()) return null;
  return { name: typeof r.name === "string" && r.name ? r.name : null, text: r.plan };
}

/** A plan document, shown in full so it can be read and approved where it appears. */
export function PlanDocCard({ name, text }: { name: string | null; text: string }) {
  const [open, setOpen] = useState(true);
  return (
    <section aria-label="Plan" className="overflow-hidden rounded-xl border border-line bg-panel/60">
      <div className="flex items-center gap-2 px-3.5 py-2">
        <button className="flex min-w-0 flex-1 items-center gap-2 text-left" onClick={() => setOpen(!open)} aria-expanded={open}>
          <span className="flex shrink-0 text-faint">
            <FileTextIcon size={15} />
          </span>
          <span className="shrink-0 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">Plan</span>
          {name && <span className="min-w-0 truncate text-[13px] text-fg/85">{name}</span>}
          <span className={`ml-auto flex shrink-0 text-faint transition-transform ${open ? "rotate-90" : ""}`}>
            <ChevronRightIcon size={12} />
          </span>
        </button>
        <CopyButton text={text} label="Copy plan" icon={<CopyIcon size={14} />} />
      </div>
      {open && (
        <div className="border-t border-line px-3.5 py-3">
          <Markdown text={text} />
        </div>
      )}
    </section>
  );
}

/** Consecutive tool calls share one box with hairlines between rows. */
export function ToolGroup({ children }: { children: ReactNode }) {
  return <div className="divide-y divide-line overflow-hidden rounded-xl border border-line">{children}</div>;
}

const OPTION_STYLE: Record<string, string> = {
  allow_once: "bg-accent font-semibold text-accent-fg hover:brightness-105",
  allow_always: "border border-border bg-panel text-fg hover:bg-panel-2",
  reject_once: "text-danger hover:bg-danger/10",
  reject_always: "text-danger hover:bg-danger/10",
};

const KIND_NOUN: Record<string, string> = {
  read: "file reads",
  edit: "file edits",
  delete: "deletions",
  move: "moves and renames",
  search: "searches",
  fetch: "web fetches",
  think: "thinking tools",
};

function commandOf(tc: AcpToolCall): string | null {
  const raw = (tc.rawInput ?? {}) as Record<string, unknown>;
  for (const k of ["command", "cmd"]) {
    const v = raw[k];
    if (typeof v === "string" && v.trim()) return v.trim();
    if (Array.isArray(v) && v.length) return v.filter((x) => typeof x === "string").join(" ");
  }
  return null;
}

/** The "always allow" rule this request suggests; mirrors the backend's matching rules. */
export function suggestRule(tc: AcpToolCall): { kind: string; pattern: string; label: string } | null {
  const kind = tc.kind;
  if (!kind || kind === "other" || kind === "switch_mode") return null;
  if (kind !== "execute") return { kind, pattern: "", label: `Always allow ${KIND_NOUN[kind] ?? kind} here` };
  const cmd = commandOf(tc);
  // Chained or redirected commands never match a rule, so don't offer one.
  if (!cmd || /[\n;&|`<>]|\$\(/.test(cmd)) return null;
  const [first, second] = cmd.split(/\s+/);
  const pattern = second && !second.startsWith("-") ? `${first} ${second}` : first;
  return { kind, pattern, label: `Always allow \`${pattern}\` here` };
}

export function PermissionCard({
  requestId,
  request,
  live,
  cwd,
}: {
  requestId: string;
  request: AcpPermissionRequest;
  live?: AcpToolCall;
  /** Folder an "always allow" rule is saved for. */
  cwd: string;
}) {
  // The request is what the user is approving: its fields win over what was stored before.
  const tc = mergeToolCall(live, request.toolCall);
  const rule = suggestRule(tc);
  // The session id routes the answer to whichever node runs the agent.
  const respond = (optionId: string) =>
    api
      .codePermissionRespond(requestId, optionId, useStore.getState().activeCodeSessionId ?? undefined)
      .catch((e) => useStore.getState().setToast(errorText(e)));
  const allowId =
    request.options.find((o) => o.kind === "allow_once")?.optionId ??
    request.options.find((o) => o.kind === "allow_always")?.optionId;
  const alwaysAllow = async () => {
    if (!rule || !allowId) return;
    try {
      await api.addPermissionRule(cwd, rule.kind, rule.pattern);
      useStore.getState().setToast("Saved. Manage rules in Settings → Coding agents.");
    } catch (e) {
      useStore.getState().setToast(errorText(e));
      return;
    }
    respond(allowId);
  };
  const cmd = tc.kind === "execute" ? commandOf(tc) : null;
  const plan = planDocOf(tc);
  // The one-off "allow" leads as the primary button; broader allows follow it.
  const allows = request.options
    .filter((o) => !o.kind.startsWith("reject"))
    .sort((x, y) => Number(x.kind !== "allow_once") - Number(y.kind !== "allow_once"));
  const rejects = request.options.filter((o) => o.kind.startsWith("reject"));
  const btn = (o: (typeof request.options)[number]) => (
    <button
      key={o.optionId}
      className={`h-[34px] rounded-lg px-3.5 text-xs ${OPTION_STYLE[o.kind] ?? "border border-border bg-panel"}`}
      onClick={() => respond(o.optionId)}
    >
      {o.name}
    </button>
  );
  return (
    <section
      aria-label="Permission required"
      className="flex flex-col gap-3 rounded-xl border border-accent/45 bg-accent/[0.06] p-3.5"
    >
      <div className="flex items-center gap-2">
        <span className="signal-pulse h-2 w-2 rounded-full bg-accent" />
        <span className="text-xs font-medium text-accent-ink">
          {cmd ? "Wants to run a command" : "Permission required"}
        </span>
      </div>
      {cmd ? (
        <CommandBlock cmd={cmd} />
      ) : plan ? (
        // A plan waiting for approval reads the same as it does once answered.
        <PlanDocCard name={plan.name} text={plan.text} />
      ) : (
        <>
          <div className="text-sm font-medium">{tc.title || tc.kind || "Tool call"}</div>
          <ToolCallBody tc={tc} />
        </>
      )}
      <div className="flex flex-wrap items-center gap-2">
        {allows.map(btn)}
        {rule && allowId && (
          <button
            className="h-[34px] rounded-lg border border-border bg-panel px-3 text-xs text-fg hover:bg-panel-2"
            onClick={alwaysAllow}
            title={`Saved in Patchbay for ${cwd}, so it applies to every session and agent in this folder`}
          >
            {rule.label.split("`").map((part, i) =>
              i % 2 ? (
                <code key={i} className="font-mono text-[12px]">
                  {part}
                </code>
              ) : (
                part
              ),
            )}
          </button>
        )}
        <span className="flex-1" />
        {rejects.map(btn)}
      </div>
    </section>
  );
}
