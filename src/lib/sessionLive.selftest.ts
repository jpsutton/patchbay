/**
 * Checks for the sidebar row's status.
 * Run: node --experimental-strip-types src/lib/sessionLive.selftest.ts
 */
import { liveNeedsYou, rowStatus, type CodeLive, type RowOwn } from "./sessionLive.ts";

function assert(ok: boolean, what: string) {
  if (!ok) throw new Error(`sessionLive selftest failed: ${what}`);
}

const own = (o: Partial<RowOwn> = {}): RowOwn => ({
  status: "ready",
  busy: false,
  permissions: [],
  questions: [],
  stalled: null,
  ...o,
});
const live = (l: Partial<CodeLive> = {}): CodeLive => ({ status: "ready", busy: false, permissions: [], last: 0, ...l });

// Nothing known about the session: stopped.
let r = rowStatus(undefined, undefined);
assert(!r.running && !r.busy && !r.attention && !r.stalled, "no state, no report: stopped");

// A session nobody opened, still running in the backend mid-turn.
r = rowStatus(undefined, live({ busy: true }));
assert(r.running && r.busy, "the report shows a running, busy session");

// Stray live events leave an idle placeholder: it must not hide the report.
r = rowStatus(own({ status: "idle", busy: true }), live({ busy: false }));
assert(r.running && !r.busy, "an idle placeholder defers to the report");

// A real state wins over the report.
r = rowStatus(own({ status: "exited" }), live({ busy: true }));
assert(!r.running && !r.busy, "the window's own exited state wins");
r = rowStatus(own({ busy: true, stalled: 600 }), live({ status: "exited" }));
assert(r.running && r.busy && r.stalled, "the window's own state wins, with its stall mark");

// A session that exited in the backend is stopped, whatever its last flags.
r = rowStatus(undefined, live({ status: "exited", busy: true }));
assert(!r.running && !r.busy, "an exited report is stopped");

// Attention comes from either source.
r = rowStatus(undefined, live({ permissions: ["p1"] }));
assert(r.attention, "an open permission in the report");
r = rowStatus(undefined, live({ questions: ["q1"] }));
assert(r.attention, "an open question in the report");
r = rowStatus(own({ questions: [{}] }), undefined);
assert(r.attention, "an open question in the window's own state");

// A signed-out session stays neutral.
r = rowStatus(own({ busy: true, auth: {} }), live());
assert(!r.running && !r.busy && !r.stalled, "signed out: no light");

// The report's stall mark only counts mid-turn.
r = rowStatus(undefined, live({ stalled: true, busy: false }));
assert(!r.stalled, "a stall mark without a turn is ignored");
r = rowStatus(undefined, live({ stalled: true, busy: true }));
assert(r.stalled, "a stalled turn");

assert(liveNeedsYou(live({ permissions: ["p"] })), "needs you: permission");
assert(!liveNeedsYou(live()), "needs you: nothing open");
assert(!liveNeedsYou(undefined), "needs you: no report");

console.log("sessionLive selftest ok");
