/**
 * Steer clearing with waitFor (pb-20261107-31 / PR #363).
 * Run: node --experimental-strip-types src/lib/readSteers.selftest.ts
 *
 * Pins what #363 actually implements:
 * 1. A steer that arrived while tools ran is held until every waitFor id is done.
 * 2. waitFor is a snapshot at arrival — a new running tool mid-wait does not refresh it
 *    (advisor Option E said "re-collects"; the code does not — pin the snapshot).
 * 3. Empty / missing waitFor falls back to the old stepped rule.
 */
import type { AcpToolCall, CodeState, Entry } from "./store";
import { readSteers } from "./readSteers.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(`readSteers selftest failed: ${msg}`);
}

const state = (entries: Entry[], toolCalls: Record<string, Partial<AcpToolCall>>): CodeState =>
  ({ entries, toolCalls }) as CodeState;

function pendingOf(s: CodeState): Extract<Entry, { kind: "user" }> | undefined {
  const u = s.entries.find((x) => x.kind === "user");
  return u && u.kind === "user" ? u : undefined;
}

/** Mirrors store's isRunning filter used when a steer snapshots waitFor. */
function runningToolIds(toolCalls: Record<string, Partial<AcpToolCall> & { toolCallId?: string }>): string[] {
  return Object.values(toolCalls)
    .filter((tc) => !tc.background && (tc.status === "pending" || tc.status === "in_progress"))
    .map((tc) => tc.toolCallId!)
    .filter(Boolean);
}

// --- 1. Held until every waitFor id is done (not cleared by partial completion) ---
{
  const s0 = state(
    [
      { kind: "tool", id: "a" },
      { kind: "tool", id: "b" },
      { kind: "user", text: "steer", pending: true, waitFor: ["a", "b"] },
    ],
    { a: { status: "completed" }, b: { status: "in_progress" } },
  );
  const afterPartial = readSteers(s0);
  assert(pendingOf(afterPartial)?.pending === true, "partial completion keeps the steer pending");
  assert(JSON.stringify(pendingOf(afterPartial)?.waitFor) === JSON.stringify(["a", "b"]), "waitFor kept while pending");

  const s1 = state(
    [
      { kind: "tool", id: "a" },
      { kind: "tool", id: "b" },
      { kind: "user", text: "steer", pending: true, waitFor: ["a", "b"] },
    ],
    { a: { status: "completed" }, b: { status: "completed" } },
  );
  const afterAll = readSteers(s1);
  assert(pendingOf(afterAll)?.pending === false, "all waitFor ids done clears pending");
  assert(pendingOf(afterAll)?.waitFor === undefined, "waitFor cleared with pending");
}

// --- 2. Snapshot: a new tool mid-wait does not expand waitFor; clearing still uses the original set ---
{
  const atArrival = runningToolIds({
    a: { toolCallId: "a", status: "in_progress" },
    b: { toolCallId: "b", status: "in_progress" },
  });
  assert(JSON.stringify(atArrival.sort()) === JSON.stringify(["a", "b"]), "arrival snapshot is the running tools");

  const midWaitRunning = runningToolIds({
    a: { toolCallId: "a", status: "completed" },
    b: { toolCallId: "b", status: "in_progress" },
    c: { toolCallId: "c", status: "in_progress" },
  });
  assert(midWaitRunning.includes("c"), "a new tool is running mid-wait");
  assert(!midWaitRunning.includes("a"), "completed tools leave the running set");

  const s = state(
    [
      { kind: "tool", id: "a" },
      { kind: "tool", id: "b" },
      { kind: "user", text: "steer", pending: true, waitFor: ["a", "b"] },
      { kind: "tool", id: "c" },
    ],
    {
      a: { status: "completed" },
      b: { status: "completed" },
      c: { status: "in_progress" },
    },
  );
  const out = readSteers(s);
  assert(pendingOf(out)?.pending === false, "clears on the original waitFor even if a newer tool is still running");
}

// --- 3. Empty waitFor falls back to the old stepped rule ---
{
  const noStep = state(
    [
      { kind: "tool", id: "a" },
      { kind: "user", text: "steer", pending: true, waitFor: [] },
    ],
    { a: { status: "completed" } },
  );
  assert(pendingOf(readSteers(noStep))?.pending === true, "empty waitFor + tool only above: still pending (no step after)");

  const stepped = state(
    [
      { kind: "user", text: "steer", pending: true, waitFor: [] },
      { kind: "tool", id: "later" },
    ],
    { later: { status: "completed" } },
  );
  assert(pendingOf(readSteers(stepped))?.pending === false, "empty waitFor clears via stepped tool after the message");

  const missing = state(
    [
      { kind: "user", text: "steer", pending: true },
      { kind: "tool", id: "later" },
    ],
    { later: { status: "completed" } },
  );
  assert(pendingOf(readSteers(missing))?.pending === false, "missing waitFor still clears via stepped");

  const missingHeld = state(
    [
      { kind: "tool", id: "a" },
      { kind: "user", text: "steer", pending: true },
    ],
    { a: { status: "completed" } },
  );
  assert(pendingOf(readSteers(missingHeld))?.pending === true, "missing waitFor + only tools above: held (old rule)");
}

console.log("readSteers.selftest: ok");
