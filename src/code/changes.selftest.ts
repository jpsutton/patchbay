/**
 * Pure-function checks for the Changes panel's diff merging and OpenCode's to-do plans.
 * Run: node --experimental-strip-types src/code/changes.selftest.ts
 */
import type { AcpToolCall, AcpToolCallContent } from "../lib/ipc";
import type { Entry } from "../lib/store";
import { collectChanges, normalizePath, type FileChange } from "./changes.ts";
import { todoPlanOf } from "../lib/todoPlan.ts";

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

// Hunks that can't be placed stay separate rather than paired.
{
  const c = changes(
    { content: [diff(`${CWD}/h.ts`, "one", "ONE")] },
    { content: [diff(`${CWD}/h.ts`, "two", "TWO")] },
  );
  eq(c[0].parts.length, 2, "unplaced fragments stay apart");
  eq(c[0].parts.map((p) => p.oldText), ["one\n", "two\n"], "unplaced fragments keep their own old text");
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
const tw = (x: Partial<AcpToolCall>) => ({ toolCallId: "w", title: "todowrite", kind: "other", status: "pending", rawInput: {}, ...x }) as AcpToolCall;

eq(todoPlanOf(tw({})), null, "pending write: no plan");
eq(todoPlanOf(tw({ status: "in_progress", rawInput: { todos } })), null, "proposed list: no plan");
eq(todoPlanOf(tw({ status: "failed", rawInput: { todos } })), null, "rejected write: no plan");
eq(
  todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos: [] }, rawOutput: { metadata: { todos } } })),
  todos,
  "completed write: saved list, cancelled kept",
);
eq(todoPlanOf(tw({ status: "completed", title: "2 todos", rawInput: { todos } })), todos, "replayed write: list from rawInput");
eq(todoPlanOf(tw({ status: "completed", title: "0 todos", rawOutput: { metadata: { todos: [] } } })), [], "empty list clears");
eq(
  todoPlanOf(tw({ status: "completed", title: "1 todos", rawOutput: { metadata: { todos: [{ content: "x" }] } } })),
  [{ content: "x", status: "pending", priority: "medium" }],
  "missing fields default",
);
eq(todoPlanOf(tw({ status: "completed", title: "Read file", rawInput: { todos } })), null, "other tools: no plan");

console.log("changes selftest: ok");
