import type { AcpPermissionRequest, AcpToolCall } from "../lib/ipc.ts";

type TemplateMatch = { ok?: boolean; label?: string; reason?: string };
type PatchbayMeta = {
  workerGitPrompt?: boolean;
  command?: string;
  cwd?: string;
  commandArgv?: string[];
  templateMatch?: TemplateMatch;
};

function rawCommandFromRequest(request: AcpPermissionRequest): {
  command: string | null;
  commandArgv: string[] | null;
  cwd: string | null;
} {
  const raw = (request.toolCall as AcpToolCall & { rawInput?: { command?: unknown; cwd?: unknown } }).rawInput;
  const cwd = typeof raw?.cwd === "string" ? raw.cwd : null;
  const cmd = raw?.command;
  if (Array.isArray(cmd)) {
    const argv = cmd.filter((a): a is string => typeof a === "string");
    if (!argv.length) return { command: null, commandArgv: null, cwd };
    return {
      commandArgv: argv,
      command: `argv: [${argv.map((a) => JSON.stringify(a)).join(", ")}]`,
      cwd,
    };
  }
  if (typeof cmd === "string" && cmd.trim()) {
    return { command: cmd.trim(), commandArgv: null, cwd };
  }
  return { command: null, commandArgv: null, cwd };
}

/**
 * Pure view of a permission request for PermissionCard and selftests.
 * Worker git (C2 / G2 / S2): trust ONLY host `_meta.patchbay` on the **permission request**
 * the host enriched — never agent top-level patchbay*, and never the live tool-call cache.
 * Absent host meta ⇒ raw `rawInput` command with an unverified label (no workerGit / template badge).
 */
export function permissionCardView(
  request: AcpPermissionRequest,
  _live?: AcpToolCall,
): {
  workerGit: boolean;
  unverified: boolean;
  actions: { kind: string; optionId: string; name: string }[];
  command: string | null;
  cwd: string | null;
  cwdDisplay: string | null;
  askReason: string | null;
  showAlwaysRule: boolean;
  templateMatch: TemplateMatch | null;
  commandArgv: string[] | null;
} {
  // Host metadata only from the permission request (S2). Do not fall back to live-call `_meta`.
  const host = (request.toolCall as AcpToolCall & { _meta?: { patchbay?: PatchbayMeta } })._meta?.patchbay;
  const workerGit = !!host?.workerGitPrompt;
  const unverified = !host;
  let command: string | null = null;
  let commandArgv: string[] | null = null;
  let cwd: string | null = null;
  if (host) {
    if (host.commandArgv && host.commandArgv.length) {
      commandArgv = host.commandArgv;
      command = `argv: [${host.commandArgv.map((a) => JSON.stringify(a)).join(", ")}]`;
    } else if (host.command?.trim()) {
      command = host.command.trim();
    }
    cwd = typeof host.cwd === "string" ? host.cwd : null;
  } else {
    const raw = rawCommandFromRequest(request);
    command = raw.command;
    commandArgv = raw.commandArgv;
    cwd = raw.cwd;
  }
  const cwdDisplay = workerGit
    ? cwd && cwd.trim()
      ? cwd.trim()
      : "unavailable"
    : cwd && cwd.trim()
      ? cwd.trim()
      : null;
  const allows = request.options
    .filter((o) => o.kind === "allow_once" || (!workerGit && !o.kind?.startsWith("reject")))
    .filter((o) => !(workerGit && o.kind === "allow_always"))
    .sort((x, y) => Number(x.kind !== "allow_once") - Number(y.kind !== "allow_once"));
  const rejects = request.options.filter((o) =>
    workerGit ? o.kind === "reject_once" : o.kind?.startsWith("reject") && o.kind !== "reject_always",
  );
  const actions = [...allows, ...rejects].map((o) => ({
    kind: o.kind ?? "",
    optionId: o.optionId,
    name: o.name ?? "",
  }));
  const allowOnce = request.options.some((o) => o.kind === "allow_once");
  const askReason = request.toolCall.askReason?.trim() || null;
  return {
    workerGit,
    unverified,
    actions,
    command,
    cwd,
    cwdDisplay,
    askReason,
    showAlwaysRule: !workerGit && allowOnce,
    // Never a template / workerGit badge without host enrich (S2).
    templateMatch: host?.templateMatch ?? (unverified ? { ok: false, label: "unverified by Patchbay" } : null),
    commandArgv,
  };
}
