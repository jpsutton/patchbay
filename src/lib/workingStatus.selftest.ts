/**
 * Pure-function checks for the working-status readout.
 * Run: node --experimental-strip-types src/lib/workingStatus.selftest.ts
 */
import type { AcpToolCall, AcpUpdate } from "./ipc";
import {
  FLOWING_MS,
  QUIET_MS,
  STALL_MS,
  chunkChars,
  fmtElapsed,
  phaseOf,
  workingStatus,
} from "./workingStatus.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const now = 1_000_000;
const text = (sessionUpdate: string, t: string) =>
  ({ sessionUpdate, content: { type: "text", text: t } }) as unknown as AcpUpdate;

assert(phaseOf(text("agent_thought_chunk", "x")) === "thinking", "thought chunk is thinking");
assert(phaseOf(text("agent_message_chunk", "x")) === "writing", "message chunk is writing");
assert(phaseOf({ sessionUpdate: "tool_call", toolCallId: "t" } as AcpUpdate) === "tool", "tool_call is tool");
assert(phaseOf({ sessionUpdate: "plan", entries: [] } as AcpUpdate) === "plan", "plan is plan");
assert(phaseOf({ sessionUpdate: "current_mode_update", currentModeId: "x" } as AcpUpdate) === null, "mode change has no phase");

assert(chunkChars(text("agent_message_chunk", "hello")) === 5, "counts message text");
assert(chunkChars(text("agent_thought_chunk", "hmm")) === 3, "counts thought text");
assert(chunkChars({ sessionUpdate: "plan", entries: [] } as AcpUpdate) === 0, "plan has no chars");
assert(
  chunkChars({ sessionUpdate: "agent_message_chunk", content: { type: "image" } } as unknown as AcpUpdate) === 0,
  "image chunk has no chars",
);

const base = { toolCalls: {}, lastForegroundAt: null, lastPhase: null, turnStartedAt: null, turnChars: 0 };
const running = { toolCallId: "t1", status: "in_progress", title: "cargo build" } as AcpToolCall;
const bg = { toolCallId: "t2", status: "completed", background: true } as AcpToolCall;

// Streaming text.
let w = workingStatus({ ...base, lastForegroundAt: now - 500, lastPhase: "thinking", turnStartedAt: now - 9000 }, now);
assert(w.kind === "thinking" && !w.quiet && w.turnMs === 9000, "recent thought chunk is thinking");
w = workingStatus({ ...base, lastForegroundAt: now - 500, lastPhase: "writing", turnStartedAt: now - 9000 }, now);
assert(w.kind === "writing", "recent message chunk is writing");

// Between steps.
w = workingStatus({ ...base, lastForegroundAt: now - FLOWING_MS - 1, lastPhase: "writing", turnStartedAt: now - 9000 }, now);
assert(w.kind === "waiting" && !w.quiet, "past FLOWING_MS the model is waiting, not writing");
w = workingStatus({ ...base, lastForegroundAt: now - 500, lastPhase: "tool", turnStartedAt: now - 9000 }, now);
assert(w.kind === "waiting", "a finished tool leaves the agent waiting for the model");

// Quiet then stalled.
w = workingStatus({ ...base, lastForegroundAt: now - QUIET_MS, lastPhase: "writing", turnStartedAt: now - 60_000 }, now);
assert(w.kind === "waiting" && w.quiet && w.idleMs === QUIET_MS, "quiet shows the counter");
w = workingStatus({ ...base, lastForegroundAt: now - STALL_MS, lastPhase: "writing", turnStartedAt: now - 60_000 }, now);
assert(w.kind === "stalled" && w.quiet, "STALL_MS of silence is stalled");

// A running tool explains silence: never quiet or stalled.
w = workingStatus({ ...base, toolCalls: { t1: running }, lastForegroundAt: now - 120_000, turnStartedAt: now - 130_000 }, now);
assert(w.kind === "tool" && w.tool === running && !w.quiet, "running tool is not stalled");
// Background tools don't count as foreground work.
w = workingStatus({ ...base, toolCalls: { t2: bg }, lastForegroundAt: now - 10_000, turnStartedAt: now - 20_000 }, now);
assert(w.kind === "waiting" && w.tool === null, "background tool is not the status");

// Idle counts from the turn start when the last update is from an earlier turn.
w = workingStatus({ ...base, lastForegroundAt: now - 500_000, lastPhase: "writing", turnStartedAt: now - 2000 }, now);
assert(w.idleMs === 2000 && w.kind === "waiting", "old update does not make a new turn stalled");
// No reference point at all (autonomous follow-up before any update).
w = workingStatus(base, now);
assert(w.idleMs === null && w.turnMs === null && !w.quiet && w.kind === "waiting", "no reference, no counters");

// Token estimate.
w = workingStatus({ ...base, turnChars: 4000 }, now);
assert(w.tokens === 1000, "4 chars per token");

assert(fmtElapsed(8_400) === "8s", "seconds");
assert(fmtElapsed(134_000) === "2m 14s", "minutes");
assert(fmtElapsed(125_000) === "2m 05s", "minutes pad seconds");
assert(fmtElapsed(3_900_000) === "1h 05m", "hours");

console.log("workingStatus.selftest: ok");
