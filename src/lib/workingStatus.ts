/**
 * What a busy Code session is doing right now, so the UI can tell "the model is slow" from
 * "nothing is happening". Pure: the store keeps the raw facts, this reads them.
 */

import type { AcpToolCall, AcpUpdate } from "./ipc";

/** Text chunks this recent count as "streaming now"; after that the model is between steps. */
export const FLOWING_MS = 3000;
/** Silence this long is worth showing as a counter. */
export const QUIET_MS = 5000;
/** Silence this long, with no tool running, is worth a warning (and a Stop button). */
export const STALL_MS = 30_000;
/** Rough characters per token, for the live counter. */
export const CHARS_PER_TOKEN = 4;

/** What the agent last did in the foreground. */
export type Phase = "thinking" | "writing" | "tool" | "plan";

/** The phase a foreground update puts the agent in; null for updates that say nothing about it. */
export function phaseOf(u: AcpUpdate): Phase | null {
  switch (u.sessionUpdate) {
    case "agent_thought_chunk":
      return "thinking";
    case "agent_message_chunk":
      return "writing";
    case "tool_call":
    case "tool_call_update":
      return "tool";
    case "plan":
      return "plan";
    default:
      return null;
  }
}

/** Characters of text a message or thought chunk carries (0 for anything else). */
export function chunkChars(u: AcpUpdate): number {
  if (u.sessionUpdate !== "agent_message_chunk" && u.sessionUpdate !== "agent_thought_chunk") return 0;
  const c = (u as { content?: { type?: string; text?: string } }).content;
  return c?.type === "text" && typeof c.text === "string" ? c.text.length : 0;
}

export type StatusKind = "thinking" | "writing" | "tool" | "waiting" | "stalled";

export interface WorkingStatus {
  kind: StatusKind;
  /** The foreground tool in flight, for `kind: "tool"`. */
  tool: AcpToolCall | null;
  /** Ms since anything arrived from the agent this turn; null when there's no reference point. */
  idleMs: number | null;
  /** Show the idle counter: quiet for a while and no tool to explain it. */
  quiet: boolean;
  /** Ms since the turn started; null outside a prompt turn. */
  turnMs: number | null;
  /** Estimated tokens streamed this turn (text and thoughts). */
  tokens: number;
}

export function workingStatus(
  s: {
    toolCalls: Record<string, AcpToolCall>;
    lastForegroundAt: number | null;
    lastPhase: Phase | null;
    turnStartedAt: number | null;
    turnChars: number;
  },
  now = Date.now(),
): WorkingStatus {
  let tool: AcpToolCall | null = null;
  for (const tc of Object.values(s.toolCalls)) {
    if (!tc.background && (tc.status === "pending" || tc.status === "in_progress")) tool = tc;
  }
  // Updates from before this turn say nothing about it.
  const ref = Math.max(s.lastForegroundAt ?? 0, s.turnStartedAt ?? 0);
  const fresh = s.lastForegroundAt != null && s.lastForegroundAt >= (s.turnStartedAt ?? 0);
  const idleMs = ref > 0 ? Math.max(0, now - ref) : null;
  const turnMs = s.turnStartedAt != null ? Math.max(0, now - s.turnStartedAt) : null;
  const tokens = Math.round(s.turnChars / CHARS_PER_TOKEN);

  let kind: StatusKind;
  if (tool) kind = "tool";
  else if (
    fresh &&
    idleMs != null &&
    idleMs < FLOWING_MS &&
    (s.lastPhase === "thinking" || s.lastPhase === "writing")
  ) {
    kind = s.lastPhase;
  } else if (idleMs != null && idleMs >= STALL_MS) kind = "stalled";
  else kind = "waiting";

  return { kind, tool, idleMs, quiet: !tool && idleMs != null && idleMs >= QUIET_MS, turnMs, tokens };
}

/** "8s", "2m 14s", "1h 05m". */
export function fmtElapsed(ms: number): string {
  const sec = Math.floor(ms / 1000);
  if (sec < 60) return `${sec}s`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ${String(sec % 60).padStart(2, "0")}s`;
  return `${Math.floor(min / 60)}h ${String(min % 60).padStart(2, "0")}m`;
}
