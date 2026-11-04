/**
 * Pure-function checks for the subagent rows.
 * Run: node --experimental-strip-types src/lib/subagents.selftest.ts
 */
import type { Subagent } from "./ipc";
import {
  BLOCKED_SECS,
  FAILED_SHOWN_SECS,
  STALL_SECS,
  applySpawned,
  applyState,
  elapsedSecs,
  health,
  idleSecs,
  shown,
  stampFailed,
  stoppable,
  subagentIdOf,
} from "./subagents.ts";

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

const stamped = stampFailed([], [agent({ id: "f", status: "failed" }), agent({ id: "r" })], t0);
assert(stamped[0].failedAt === t0 && stamped[1].failedAt === undefined, "a newly failed row gets failedAt");
const restamped = stampFailed(stamped, [agent({ id: "f", status: "failed" })], t0 + 30_000);
assert(restamped[0].failedAt === t0, "later updates keep the first failedAt");
assert(shown(restamped, t0 + 30_000).length === 1, "a just-failed row shows");
assert(shown(restamped, t0 + FAILED_SHOWN_SECS * 1000).length === 0, "a failed row leaves after a minute");

assert(stoppable(agent()), "a running background agent can be stopped");
assert(!stoppable(agent({ background: false })), "a foreground one cannot");
assert(!stoppable(agent({ status: "completed" })), "a finished one cannot");

// Cursor / Grok ACP announcements
assert(subagentIdOf({ subagentSessionId: "c1" }) === "c1", "Cursor id");
assert(subagentIdOf({ child_session_id: "g1", subagent_id: "sa" }) === "g1", "Grok prefers child_session_id");
assert(subagentIdOf({ subagent_id: "sa" }) === "sa", "Grok subagent_id fallback");

const spawned = applySpawned([], {
  sessionUpdate: "subagent_spawned",
  subagentSessionId: "30e89cbf",
  name: "generalPurpose",
  task: "Investigate issue #234",
  capabilities: {},
}, t0);
assert(spawned?.length === 1, "spawn adds a row");
assert(spawned![0].origin === "session", "spawn is session-origin");
assert(spawned![0].name === "generalPurpose", "spawn name");
assert(spawned![0].current === "Investigate issue #234", "spawn task becomes current");
assert(spawned![0].startedAt === t0, "spawn records startedAt");
assert(health(spawned![0], STALL_SECS) === "active", "ACP rows never go quiet from idle alone");
assert(idleSecs(spawned![0], t0, t0 + 60_000) === 0, "ACP idle stays zero");
assert(elapsedSecs(spawned![0], null, t0 + 45_000) === 45, "ACP elapsed grows from startedAt");
assert(!stoppable(spawned![0]), "ACP rows are not stoppable via patchbay cancel");

const done = applyState(spawned!, {
  sessionUpdate: "subagent_state_update",
  subagentSessionId: "30e89cbf",
  state: "completed",
}, t0 + 45_000);
assert(done![0].status === "completed", "state_update completes");
assert(done![0].elapsed_secs === 45, "finish freezes elapsed");
assert(done![0].startedAt === undefined, "finish clears startedAt");
assert(shown(done!).length === 0, "completed ACP rows leave the panel");

const failed = applyState(spawned!, {
  sessionUpdate: "subagent_state_update",
  subagentSessionId: "30e89cbf",
  state: "disconnected",
}, t0 + 10_000);
assert(failed![0].status === "failed" && failed![0].error === "Disconnected", "disconnected maps to failed");
assert(shown(failed!, t0 + 10_000).map((a) => a.id).join() === "30e89cbf", "failed ACP rows stay visible");
assert(failed![0].failedAt === t0 + 10_000, "failing stamps failedAt");
assert(shown(failed!, t0 + 10_000 + FAILED_SHOWN_SECS * 1000).length === 0, "then leave after a minute");

const grok = applySpawned([], {
  sessionUpdate: "subagent_spawned",
  subagent_id: "sa",
  child_session_id: "child-a",
  subagent_type: "general-purpose",
  description: "task",
}, t0);
assert(grok![0].id === "child-a" && grok![0].name === "general-purpose", "Grok spawn shape");
const grokDone = applyState(grok!, {
  sessionUpdate: "subagent_finished",
  subagent_id: "sa",
  child_session_id: "child-a",
  status: "completed",
}, t0 + 5_000);
assert(grokDone![0].status === "completed", "Grok finish via status");

assert(applySpawned([], { sessionUpdate: "subagent_spawned" }) === null, "spawn without id is ignored");
assert(applyState([], { sessionUpdate: "subagent_state_update", subagentSessionId: "x" }) === null, "state without state is ignored");

console.log("subagents.selftest: ok");
