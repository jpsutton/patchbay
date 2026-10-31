/**
 * How the subagents of Patchbay's own agent are doing, for the row each gets under the
 * transcript. Pure: the store keeps the last `subagents_update` and when it arrived.
 */

import type { Subagent } from "./ipc";

/** Idle this long, a running subagent counts as stalled (the same as the agent's own limit). */
export const STALL_SECS = 300;
/** The same when it waits on the user, who is the only one who can get it going. */
export const BLOCKED_SECS = 120;

export type Health = "active" | "quiet" | "waiting" | "done" | "failed" | "stopped";

/** Seconds since it last did something: as reported, plus the time since the report arrived. */
export function idleSecs(a: Subagent, receivedAt: number | null, now = Date.now()): number {
  if (a.status !== "running" || receivedAt == null) return a.idle_secs;
  return a.idle_secs + Math.max(0, (now - receivedAt) / 1000);
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
      if (a.blocked) return idle >= BLOCKED_SECS ? "waiting" : "active";
      return idle >= STALL_SECS ? "quiet" : "active";
  }
}

/** The subagents worth a row: those still running, and the ones that just failed. */
export function shown(agents: Subagent[]): Subagent[] {
  return agents.filter((a) => a.status === "running" || a.status === "failed");
}

/** Whether the row can offer Stop: only a background subagent, and only while it runs. */
export function stoppable(a: Subagent): boolean {
  return a.status === "running" && a.background;
}
