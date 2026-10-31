/**
 * Pure-function checks for the subagent rows.
 * Run: node --experimental-strip-types src/lib/subagents.selftest.ts
 */
import type { Subagent } from "./ipc";
import { BLOCKED_SECS, STALL_SECS, health, idleSecs, shown, stoppable } from "./subagents.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const agent = (over: Partial<Subagent> = {}): Subagent => ({
  id: "agent_1",
  name: "agentP",
  kind: "general-purpose",
  description: "tests",
  status: "running",
  error: null,
  background: true,
  elapsed_secs: 600,
  idle_secs: 10,
  steps: 4,
  current: "waiting for the model",
  blocked: false,
  worktree: null,
  ...over,
});

const t0 = 1_000_000;
assert(idleSecs(agent(), t0, t0 + 30_000) === 40, "idle keeps counting from when the report arrived");
assert(idleSecs(agent(), null, t0) === 10, "no arrival time: as reported");
assert(idleSecs(agent({ status: "completed" }), t0, t0 + 30_000) === 10, "a finished agent's idle does not grow");

assert(health(agent(), 10) === "active", "recent activity is active");
assert(health(agent(), STALL_SECS) === "quiet", "quiet past the stall limit");
assert(health(agent({ blocked: true }), BLOCKED_SECS - 1) === "active", "just asked is not yet waiting");
assert(health(agent({ blocked: true }), BLOCKED_SECS) === "waiting", "an unanswered prompt shows sooner");
assert(health(agent({ blocked: true }), STALL_SECS) === "waiting", "and stays waiting");
assert(health(agent({ status: "failed" }), 0) === "failed", "failed");
assert(health(agent({ status: "stopped" }), 0) === "stopped", "stopped");
assert(health(agent({ status: "completed" }), 0) === "done", "done");

const list = [agent({ id: "a" }), agent({ id: "b", status: "completed" }), agent({ id: "c", status: "failed" }), agent({ id: "d", status: "stopped" })];
assert(shown(list).map((a) => a.id).join() === "a,c", "rows for running and failed only");

assert(stoppable(agent()), "a running background agent can be stopped");
assert(!stoppable(agent({ background: false })), "a foreground one cannot");
assert(!stoppable(agent({ status: "completed" })), "a finished one cannot");

console.log("subagents.selftest: ok");
