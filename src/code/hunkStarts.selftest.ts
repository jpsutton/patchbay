/**
 * Pure-function checks for tool call diff start lines.
 * Run: node --experimental-strip-types src/code/hunkStarts.selftest.ts
 */
import type { AcpToolCallContent } from "../lib/ipc";
import { hunkStarts, lineCount } from "./hunkStarts.ts";

type Diff = Extract<AcpToolCallContent, { type: "diff" }>;

function eq(actual: unknown, expected: unknown, msg: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}

const P = "/w/src/main.ts";
const diff = (oldText: string | null, newText: string, path = P): Diff => ({ type: "diff", path, oldText, newText });

// lineCount ignores a trailing newline.
eq(lineCount(""), 0, "empty");
eq(lineCount(null), 0, "null");
eq(lineCount("a"), 1, "one line");
eq(lineCount("a\n"), 1, "one line, newline-ended");
eq(lineCount("a\nb"), 2, "two lines");
eq(lineCount("a\nb\n"), 2, "two lines, newline-ended");

// Claude Code hunks: the second hunk's old start drops by the first hunk's net added line.
eq(
  hunkStarts(
    [diff("a\nb\n\nx", "a\nb\nc\n\nx"), diff("f\n a\n b\n r\n}", "f\n a\n b\n c\n r\n}")],
    [{ path: P, line: 10 }, { path: P, line: 40 }],
  ),
  [{ newStart: 10, oldStart: 10 }, { newStart: 40, oldStart: 39 }],
  "two hunks",
);

// A whole-file diff with a location line is not offset: created, deleted.
eq(hunkStarts([diff(null, "a\nb\n")], [{ path: P, line: 5 }]), [undefined], "created file");
eq(hunkStarts([diff("a\nb\n", "")], [{ path: P, line: 5 }]), [undefined], "deleted file");
// A location without a line (Write fallback, Codex) leaves the diff at line 1.
eq(hunkStarts([diff("a\n", "b\n")], [{ path: P }]), [undefined], "no line");
eq(hunkStarts([diff("a\n", "b\n")], null), [undefined], "no locations");

// Counts must match per path, else none of that path's diffs is offset.
eq(
  hunkStarts([diff("a", "b"), diff("c", "d")], [{ path: P, line: 3 }]),
  [undefined, undefined],
  "fewer lines than diffs",
);
eq(
  hunkStarts([diff("a", "b")], [{ path: P, line: 3 }, { path: P, line: 9 }]),
  [undefined],
  "more lines than diffs",
);
// Another path's shape doesn't affect this one.
eq(
  hunkStarts([diff("a", "b"), diff("c", "d", "/w/other.ts")], [{ path: P, line: 3 }]),
  [{ newStart: 3, oldStart: 3 }, undefined],
  "per path",
);

// An old start before line 1 drops the offset rather than clamping.
eq(
  hunkStarts([diff("a", "a\nb\nc\nd"), diff("x", "y")], [{ path: P, line: 1 }, { path: P, line: 2 }]),
  [{ newStart: 1, oldStart: 1 }, undefined],
  "old start below 1",
);

console.log("hunkStarts selftest: ok");
