/**
 * Pure-function checks for the Changes panel's diff merging and OpenCode's to-do plans.
 * Run: node --experimental-strip-types src/code/changes.selftest.ts
 */
import type { AcpToolCall, AcpToolCallContent } from "../lib/ipc";
import type { Entry } from "../lib/store";
import { collectChanges, normalizePath, type FileChange } from "./changes.ts";
import { planUpdateOf, todoPlanOf } from "../lib/todoPlan.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}
const eq = (a: unknown, b: unknown, msg: string) =>
  assert(JSON.stringify(a) === JSON.stringify(b), `${msg}: got ${JSON.stringify(a)}, want ${JSON.stringify(b)}`);

const CWD = "/work/demo";
const diff = (path: string, oldText: string | null, newText: string): AcpToolCallContent => ({ type: "diff", path, oldText, newText });
let n = 0;
/** Runs `calls` through collectChanges in order. */
function changes(...calls: Partial<AcpToolCall>[]): FileChange[] {
  const toolCalls: Record<string, AcpToolCall> = {};
  const order: Entry[] = [];
  for (const c of calls) {
    const id = `t${n++}`;
    toolCalls[id] = { toolCallId: id, title: "Edit", status: "completed", ...c } as AcpToolCall;
    order.push({ kind: "tool", id } as Entry);
  }
  return collectChanges(order, toolCalls, CWD);
}

// Paths: relative, `./`, `..` and doubled slashes all land on one absolute path.
eq(normalizePath("src/a.ts", CWD), "/work/demo/src/a.ts", "relative path");
eq(normalizePath("./src//b/../a.ts", CWD), "/work/demo/src/a.ts", "dotted path");
eq(normalizePath("/work/demo/src/a.ts", CWD), "/work/demo/src/a.ts", "absolute path");
eq(normalizePath("src/a.ts", "/"), "/src/a.ts", "relative to root");
eq(normalizePath("/", "/"), "/", "root itself");
eq(normalizePath("c:\\proj\\a.ts", CWD), normalizePath("C:/proj/a.ts", CWD), "drive letter case");
eq(normalizePath("a.ts", "c:\\proj"), "C:/proj/a.ts", "relative to a drive");
eq(normalizePath("\\\\server\\share\\a.ts", CWD), "//server/share/a.ts", "UNC kept");
eq(normalizePath("//server/share/../../x", CWD), "//server/share/x", "UNC: .. stops at the share");
assert(normalizePath("\\\\server\\share\\a.ts", CWD) !== normalizePath("/server/share/a.ts", CWD), "UNC apart from POSIX");
{
  const c = changes(
    { content: [diff("src/a.ts", "x\n", "y\n")] },
    { content: [diff(`${CWD}/src/a.ts`, "y\n", "z\n")] },
  );
  eq(c.length, 1, "one file for relative and absolute paths");
  eq(c[0].parts, [{ oldText: "x\n", newText: "z\n", oldStart: 1, newStart: 1 }], "whole-file edits chain");
}

// Claude Code: one Edit, two hunks of one file, each hunk's new start line in `locations`.
const FILE = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn";
{
  const c = changes({
    content: [diff(`${CWD}/f.ts`, "b\nc\nd", "b\nC\nC2\nd"), diff(`${CWD}/f.ts`, "j\nk\nl", "j\nl")],
    locations: [{ path: `${CWD}/f.ts`, line: 2 }, { path: `${CWD}/f.ts`, line: 11 }],
  });
  eq(c.length, 1, "multi-hunk: one file");
  eq(c[0].status, "modified", "multi-hunk: modified");
  eq(
    c[0].parts,
    [
      { oldText: "b\nc\nd\n", newText: "b\nC\nC2\nd\n", oldStart: 2, newStart: 2 },
      { oldText: "j\nk\nl\n", newText: "j\nl\n", oldStart: 10, newStart: 11 },
    ],
    "multi-hunk: hunks kept apart, never first-old to last-new",
  );
}

// Claude Code: a later Edit whose hunk overlaps an earlier one merges into it.
{
  const c = changes(
    {
      content: [diff(`${CWD}/f.ts`, "b\nc\nd", "b\nC\nd")],
      locations: [{ path: `${CWD}/f.ts`, line: 2 }],
    },
    {
      content: [diff(`${CWD}/f.ts`, "C\nd\ne", "C\nD\ne")],
      locations: [{ path: `${CWD}/f.ts`, line: 3 }],
    },
  );
  eq(c[0].parts, [{ oldText: "b\nc\nd\ne\n", newText: "b\nC\nD\ne\n", oldStart: 2, newStart: 2 }], "overlapping hunks merge");
}

// Claude Code: a later hunk below an earlier one that added lines; line numbers stay right.
{
  const c = changes(
    { content: [diff(`${CWD}/f.ts`, "a\nb", "a\nnew\nb")], locations: [{ path: `${CWD}/f.ts`, line: 1 }] },
    { content: [diff(`${CWD}/f.ts`, "m\nn", "m")], locations: [{ path: `${CWD}/f.ts`, line: 14 }] },
  );
  eq(c[0].parts.map((p) => [p.oldStart, p.newStart]), [[1, 1], [13, 14]], "line numbers across hunks");
}

// Claude Code Write over a new file: `oldText: null` with the full text, never narrowed.
{
  const c = changes({ content: [diff(`${CWD}/new.md`, null, "hello\nworld\n")], locations: [{ path: `${CWD}/new.md` }] });
  eq(c[0].status, "added", "write: added");
  eq(c[0].parts, [{ oldText: null, newText: "hello\nworld\n", oldStart: 1, newStart: 1 }], "write: whole new file");
}

// Claude Code Write over an existing file: the null-old full text is replaced by hunks.
{
  const c = changes({ content: [diff(`${CWD}/f.ts`, "c\nd\ne", "c\nX\ne")], locations: [{ path: `${CWD}/f.ts`, line: 3 }] });
  eq(c[0].status, "modified", "overwrite: modified, not added");
  eq(c[0].parts.length, 1, "overwrite: one hunk");
}

// A Write still in flight (full text) followed by an Edit with a hunk in it.
{
  const c = changes(
    { content: [diff(`${CWD}/g.ts`, null, `${FILE}\n`)] },
    { content: [diff(`${CWD}/g.ts`, "c\nd\ne", "c\nD\ne")], locations: [{ path: `${CWD}/g.ts`, line: 3 }] },
  );
  eq(c[0].status, "added", "write then edit: added");
  eq(c[0].parts.length, 1, "write then edit: one part");
  eq(c[0].parts[0].oldText, null, "write then edit: no old text");
  assert(c[0].parts[0].newText.includes("\nD\n") && !c[0].parts[0].newText.includes("\nd\n"), "write then edit: edit applied");
}

// A Write over an existing file: the pending `oldText: null` full text, then (same call) its hunks.
{
  const p = `${CWD}/notes.md`;
  const c = changes({
    content: [diff(p, null, "# Notes\n- one\n- two, revised\n"), diff(p, "# Notes\n- one\n- two", "# Notes\n- one\n- two, revised")],
    locations: [{ path: p }, { path: p }],
  });
  eq(c[0].status, "modified", "write over existing: modified, not added");
  eq(c[0].parts, [{ oldText: "# Notes\n- one\n- two\n", newText: "# Notes\n- one\n- two, revised\n", oldStart: 1, newStart: 1 }], "write over existing: its hunk");
}
// The same as two frames seen as calls: the null text, then a hunk not in it.
{
  const p = `${CWD}/notes2.md`;
  const c = changes(
    { status: "pending", content: [diff(p, null, "# Notes\n- two, revised\n")], locations: [{ path: p }] },
    { content: [diff(p, "# Notes\n- two", "# Notes\n- two, revised")], locations: [{ path: p, line: 1 }] },
  );
  eq(c[0].status, "modified", "write preview then hunk: modified");
  eq(c[0].parts.length, 1, "write preview then hunk: one part");
  eq(c[0].parts[0].oldText, "# Notes\n- two\n", "write preview then hunk: real old text");
}
// A pending Write alone is still the last word: added.
eq(changes({ status: "pending", content: [diff(`${CWD}/w.md`, null, "x\n")] })[0].status, "added", "pending write alone: added");

// No line hint: text that occurs more than once isn't merged into the first occurrence.
{
  const c = changes(
    { content: [diff(`${CWD}/dup.ts`, "a\nx\nb\nx\nc", "a\nx\nb\nx\nc\nd")] },
    { content: [diff(`${CWD}/dup.ts`, "x", "y")] },
  );
  eq(c[0].parts.length, 2, "ambiguous text: separate region");
  eq(c[0].parts[0].newText, "a\nx\nb\nx\nc\nd\n", "ambiguous text: first region untouched");
  eq(c[0].parts.map((p) => p.newStart), [null, null], "ambiguous text: no made-up line numbers");
}
// No line hint, unique text: merged.
{
  const c = changes(
    { content: [diff(`${CWD}/uniq.ts`, "a\nb\nc", "a\nB\nc")] },
    { content: [diff(`${CWD}/uniq.ts`, "B", "BB")] },
  );
  eq(c[0].parts, [{ oldText: "a\nb\nc\n", newText: "a\nBB\nc\n", oldStart: 1, newStart: 1 }], "unique text: merged");
}
// A wide diff holding two earlier regions merges both, with no line twice.
{
  const c = changes(
    { content: [diff(`${CWD}/two.ts`, "one", "ONE")] },
    { content: [diff(`${CWD}/two.ts`, "two", "TWO")] },
    { content: [diff(`${CWD}/two.ts`, "ONE\nmid\nTWO", "ONE\nMID\nTWO")] },
  );
  eq(c[0].parts, [{ oldText: "one\nmid\ntwo\n", newText: "ONE\nMID\nTWO\n", oldStart: 1, newStart: 1 }], "wide diff merges every region inside");
}
// The same with known lines.
{
  const p = `${CWD}/two2.ts`;
  const c = changes(
    { content: [diff(p, "b", "B"), diff(p, "d", "D")], locations: [{ path: p, line: 2 }, { path: p, line: 4 }] },
    { content: [diff(p, "a\nB\nc\nD\ne", "a\nB\nC\nD\ne")], locations: [{ path: p, line: 1 }] },
  );
  eq(c[0].parts, [{ oldText: "a\nb\nc\nd\ne\n", newText: "a\nB\nC\nD\ne\n", oldStart: 1, newStart: 1 }], "wide hunk merges every region inside");
}

// CRLF file, LF hunk: they meet, and the text shown keeps the file's own lines.
{
  const p = `${CWD}/crlf.txt`;
  const c = changes(
    { content: [diff(p, "a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n")] },
    { content: [diff(p, "B\nc", "B\nC")], locations: [{ path: p, line: 2 }] },
  );
  eq(c[0].parts.length, 1, "crlf: merged");
  eq(c[0].parts[0].oldText, "a\r\nb\r\nc\r\n", "crlf: old text kept");
  eq(c[0].parts[0].newText, "a\r\nB\nC\n", "crlf: new text applied");
}
{
  const p = `${CWD}/crlf2.txt`;
  const c = changes(
    { content: [diff(p, "x\r\ny\r\n", "x\r\nY\r\n")] },
    { content: [diff(p, "Y", "YY")] },
  );
  eq(c[0].parts.length, 1, "crlf no hint: merged");
}

// Hunks that can't be placed stay separate rather than paired.
{
  const c = changes(
    { content: [diff(`${CWD}/h.ts`, "one", "ONE")] },
    { content: [diff(`${CWD}/h.ts`, "two", "TWO")] },
  );
  eq(c[0].parts.length, 2, "unplaced fragments stay apart");
  eq(c[0].parts.map((p) => p.oldText), ["one\n", "two\n"], "unplaced fragments keep their own old text");
  eq(c[0].parts.map((p) => [p.oldStart, p.newStart]), [[null, null], [null, null]], "unplaced fragments: no line numbers");
}

// Codex: whole files; deletion is `newText: ""`.
{
  const c = changes(
    { content: [diff(`${CWD}/gone.txt`, "bye\n", "")] },
    { content: [diff(`${CWD}/kept.txt`, "a\n", "b\n")] },
  );
  eq(c.map((f) => f.status), ["deleted", "modified"], "codex: deletion");
  eq(c[0].parts[0].newText, "", "codex: deleted file has no new text");
}
// A deletion after earlier edits (several regions) is still a deletion.
{
  const p = `${CWD}/del.txt`;
  const c = changes(
    { content: [diff(p, "b", "B"), diff(p, "d", "D")], locations: [{ path: p, line: 2 }, { path: p, line: 4 }] },
    { content: [diff(p, "a\nB\nc\nD\n", "")] },
  );
  eq(c[0].status, "deleted", "deleted after edits");
  eq(c[0].parts, [{ oldText: "a\nb\nc\nd\n", newText: "", oldStart: 1, newStart: 1 }], "deleted after edits: original text removed");
}
{
  const p = `${CWD}/del2.txt`;
  const c = changes({ content: [diff(p, "one", "ONE")] }, { content: [diff(p, "two", "TWO")] }, { content: [diff(p, "ONE\nzz\nTWO\nq\nq", "")] });
  eq(c[0].status, "deleted", "deleted with unplaced regions");
  eq(c[0].parts.length, 1, "deleted: one part");
}
// Created then deleted in one session: nothing to show.
eq(changes({ content: [diff(`${CWD}/tmp.txt`, null, "x\n")] }, { content: [diff(`${CWD}/tmp.txt`, "x\n", "")] }).length, 0, "created then deleted");
// Failed calls changed nothing.
eq(changes({ status: "failed", content: [diff(`${CWD}/f.ts`, "a", "b")] }).length, 0, "failed call");

// ---- OpenCode / Kilo todowrite ----

const todos = [
  { content: "Read the code", status: "completed", priority: "high" },
  { content: "Fix it", status: "in_progress", priority: "high" },
  { content: "Old idea", status: "cancelled", priority: "low" },
];
const OC = "opencode" as const;
const tw = (x: Partial<AcpToolCall>) => ({ toolCallId: "w", title: "todowrite", kind: "other", status: "pending", rawInput: {}, ...x }) as AcpToolCall;

eq(todoPlanOf(tw({}), OC), null, "pending write: no plan");
eq(todoPlanOf(tw({ status: "in_progress", rawInput: { todos } }), OC), null, "proposed list: no plan");
eq(todoPlanOf(tw({ status: "failed", rawInput: { todos } }), OC), null, "rejected write: no plan");
eq(
  todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos: [] }, rawOutput: { metadata: { todos } } }), OC),
  todos,
  "completed write: saved list, cancelled kept",
);
eq(todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos } }), OC), todos, "replayed write: list from rawInput");
eq(todoPlanOf(tw({ status: "completed", title: "0 todos", rawOutput: { metadata: { todos: [] } } }), OC), [], "empty list clears");
eq(
  todoPlanOf(tw({ status: "completed", title: "1 todos", rawOutput: { metadata: { todos: [{ content: "x" }] } } }), OC),
  [{ content: "x", status: "pending", priority: "medium" }],
  "missing fields default",
);
eq(todoPlanOf(tw({ status: "completed", title: "Read file", rawInput: { todos } }), OC), null, "other tools: no plan");

eq(todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos } }), "kilo"), todos, "kilo too");
eq(todoPlanOf(tw({ status: "completed", title: "todowrite", rawInput: { todos } }), "claude-code"), null, "other agents' todowrite: no plan");
eq(todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos } }), undefined), null, "unknown agent: no plan");

// Only the frame that completes a write sets the plan; a late repeat of an older one doesn't.
{
  const done = tw({ status: "completed", title: "2 todos", rawOutput: { metadata: { todos } } });
  const running = tw({ status: "in_progress", rawInput: { todos } });
  eq(planUpdateOf(running, done, OC), todos, "completing frame sets the plan");
  eq(planUpdateOf(undefined, done, OC), todos, "replay announce sets the plan");
  eq(planUpdateOf(done, done, OC), null, "late repeat of a completed write: no plan");
  const noOut = tw({ status: "completed", title: "2 todos", rawInput: { todos } });
  eq(planUpdateOf(noOut, done, OC), todos, "saved list arriving after completion sets it");
  eq(planUpdateOf(running, done, "codex"), null, "completing frame, other agent: no plan");
}

console.log("changes selftest: ok");
