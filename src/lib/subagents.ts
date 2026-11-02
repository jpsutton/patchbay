/**
 * How subagents are doing, for the row each gets under the transcript. Covers Patchbay's own
 * agent (`subagents_update`) and ACP spawn announcements from Cursor / Grok
 * (`subagent_spawned`, `subagent_state_update`, `subagent_finished`). Pure: the store keeps the
 * rows and when the last harness report arrived.
 */

import type { Subagent } from "./ipc";

/** Idle this long, a running subagent counts as stalled (the same as the agent's own limit). */
export const STALL_SECS = 300;
/** The same when it waits on the user, who is the only one who can get it going. */
export const BLOCKED_SECS = 120;

export type Health = "active" | "quiet" | "waiting" | "done" | "failed" | "stopped";

function str(v: unknown): string | undefined {
  return typeof v === "string" && v.length > 0 ? v : undefined;
}

/** Child session id from Cursor (`subagentSessionId`) or Grok (`child_session_id` / `subagent_id`). */
export function subagentIdOf(update: Record<string, unknown>): string | undefined {
  return str(update.subagentSessionId) ?? str(update.child_session_id) ?? str(update.subagent_id);
}

/** Map an ACP / Grok terminal state onto the row status the panel understands. */
export function statusOf(raw: string): { status: Subagent["status"]; error: string | null } {
  switch (raw) {
    case "completed":
      return { status: "completed", error: null };
    case "failed":
      return { status: "failed", error: null };
    case "cancelled":
    case "stopped":
      return { status: "stopped", error: null };
    case "disconnected":
      return { status: "failed", error: "Disconnected" };
    default:
      return { status: "failed", error: raw };
  }
}

function statusNote(status: Subagent["status"], error: string | null): string {
  switch (status) {
    case "completed":
      return "Completed";
    case "failed":
      return error ?? "Failed";
    case "stopped":
      return "Stopped";
    default:
      return "Running";
  }
}

/** Seconds since it last did something: as reported, plus the time since the report arrived. */
export function idleSecs(a: Subagent, receivedAt: number | null, now = Date.now()): number {
  // ACP announcements have no activity signal; idle would only grow from the spawn clock.
  if (a.origin === "session") return 0;
  if (a.status !== "running" || receivedAt == null) return a.idle_secs;
  return a.idle_secs + Math.max(0, (now - receivedAt) / 1000);
}

/** How long the subagent has been running, including time since the last report. */
export function elapsedSecs(a: Subagent, receivedAt: number | null, now = Date.now()): number {
  if (a.status !== "running") return a.elapsed_secs;
  if (a.origin === "session" && a.startedAt != null) {
    return a.elapsed_secs + Math.max(0, (now - a.startedAt) / 1000);
  }
  if (receivedAt == null) return a.elapsed_secs;
  return a.elapsed_secs + Math.max(0, (now - receivedAt) / 1000);
}

export function health(a: Subagent, idle: number): Health {
  switch (a.status) {
    case "completed":
      return "done";
    case "failed":
      return "failed";
    case "stopped":
      return "stopped";
    default:
      // No progress updates from ACP spawn announcements, so never mark them quiet.
      if (a.origin === "session") return "active";
      if (a.blocked) return idle >= BLOCKED_SECS ? "waiting" : "active";
      return idle >= STALL_SECS ? "quiet" : "active";
  }
}

/** The subagents worth a row: those still running, and the ones that just failed. */
export function shown(agents: Subagent[]): Subagent[] {
  return agents.filter((a) => a.status === "running" || a.status === "failed");
}

/** Whether the row can offer Stop: only a Patchbay background subagent while it runs. */
export function stoppable(a: Subagent): boolean {
  return a.status === "running" && a.background && a.origin !== "session";
}

/**
 * Fold a `subagent_spawned` update into the list. Cursor and Grok field names both work.
 * `null` when the update has no usable id.
 */
export function applySpawned(
  agents: Subagent[],
  update: Record<string, unknown>,
  now = Date.now(),
): Subagent[] | null {
  const id = subagentIdOf(update);
  if (!id) return null;
  const name = str(update.name) ?? str(update.subagent_type) ?? null;
  const task = str(update.task) ?? str(update.description) ?? "";
  const prev = agents.find((a) => a.id === id);
  const elapsed =
    prev?.status === "running" && prev.startedAt != null
      ? prev.elapsed_secs + Math.max(0, (now - prev.startedAt) / 1000)
      : (prev?.elapsed_secs ?? 0);
  const row: Subagent = {
    id,
    name,
    kind: name ?? "subagent",
    description: task || prev?.description || "",
    status: "running",
    error: null,
    background: true,
    elapsed_secs: elapsed,
    idle_secs: 0,
    steps: prev?.steps ?? 0,
    current: task || prev?.current || "Running",
    blocked: false,
    worktree: null,
    origin: "session",
    startedAt: now,
  };
  return [...agents.filter((a) => a.id !== id), row];
}

/**
 * Fold a `subagent_state_update` or Grok `subagent_finished` into the list. `null` when the
 * update has no id or no state.
 */
export function applyState(
  agents: Subagent[],
  update: Record<string, unknown>,
  now = Date.now(),
): Subagent[] | null {
  const id = subagentIdOf(update);
  const raw = str(update.state) ?? str(update.status);
  if (!id || !raw) return null;
  const { status, error } = statusOf(raw);
  const prev = agents.find((a) => a.id === id);
  if (!prev) {
    return [
      ...agents,
      {
        id,
        name: null,
        kind: "subagent",
        description: "",
        status,
        error,
        background: true,
        elapsed_secs: 0,
        idle_secs: 0,
        steps: 0,
        current: statusNote(status, error),
        blocked: false,
        worktree: null,
        origin: "session",
      },
    ];
  }
  const elapsed =
    prev.status === "running" && prev.startedAt != null
      ? prev.elapsed_secs + Math.max(0, (now - prev.startedAt) / 1000)
      : prev.elapsed_secs;
  return agents.map((a) =>
    a.id === id
      ? {
          ...a,
          status,
          error: error ?? a.error,
          elapsed_secs: elapsed,
          startedAt: undefined,
          current: status === "running" ? a.current : statusNote(status, error),
        }
      : a,
  );
}
