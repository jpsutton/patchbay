/**
 * Pure-function checks for the Working… heuristic.
 * Run: node --experimental-strip-types src/lib/codeWorking.selftest.ts
 */
import type { AcpToolCall, AcpUpdate } from "./ipc";
import {
  FOREGROUND_IDLE_MS,
  codeWorking,
  hasActiveForegroundTools,
  isForegroundUpdate,
} from "./codeWorking.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const agentChunk = { sessionUpdate: "agent_message_chunk" } as AcpUpdate;
const thoughtChunk = { sessionUpdate: "agent_thought_chunk" } as AcpUpdate;
const plan = { sessionUpdate: "plan" } as AcpUpdate;
const toolCall = { sessionUpdate: "tool_call", toolCallId: "t1" } as AcpUpdate;

assert(isForegroundUpdate(agentChunk), "agent chunk is foreground");
assert(isForegroundUpdate(thoughtChunk), "thought chunk is foreground");
assert(isForegroundUpdate(plan), "plan is foreground");
assert(isForegroundUpdate(toolCall), "tool_call is foreground");

const terminalOnly = {
  sessionUpdate: "tool_call_update",
  toolCallId: "t1",
  _meta: { terminal_output: { data: "x", truncated: false } },
} as unknown as AcpUpdate;

const completedBg: AcpToolCall = {
  toolCallId: "t1",
  title: "sleep",
  kind: "execute",
  status: "completed",
  background: true,
};
assert(
  !isForegroundUpdate(terminalOnly, completedBg),
  "terminal-only update on completed background tool is not foreground",
);

const inProgress: AcpToolCall = {
  toolCallId: "t2",
  title: "edit",
  kind: "edit",
  status: "in_progress",
};
assert(isForegroundUpdate({
  sessionUpdate: "tool_call_update",
  toolCallId: "t2",
  status: "in_progress",
} as unknown as AcpUpdate), "in_progress tool_call_update is foreground");

assert(hasActiveForegroundTools({ t2: inProgress }), "in-progress non-background counts");
assert(
  !hasActiveForegroundTools({ t1: completedBg }),
  "only background completed tools do not count",
);

const now = 100_000;
assert(
  codeWorking({ busy: true, toolCalls: {}, lastForegroundAt: null }, now),
  "busy alone is working",
);
assert(
  codeWorking({ busy: false, toolCalls: { t2: inProgress }, lastForegroundAt: null }, now),
  "active foreground tool is working",
);
assert(
  codeWorking(
    { busy: false, toolCalls: { t1: completedBg }, lastForegroundAt: now - 100 },
    now,
  ),
  "recent lastForegroundAt is working",
);
assert(
  !codeWorking(
    {
      busy: false,
      toolCalls: { t1: completedBg },
      lastForegroundAt: now - FOREGROUND_IDLE_MS - 1,
    },
    now,
  ),
  "elapsed idle window with only background tools is not working",
);
assert(
  !codeWorking({ busy: false, toolCalls: {}, lastForegroundAt: null }, now),
  "idle is not working",
);

console.log("codeWorking.selftest: ok");
