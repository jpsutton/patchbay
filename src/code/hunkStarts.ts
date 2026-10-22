import type { AcpToolCall, AcpToolCallContent } from "../lib/ipc";

type Diff = Extract<AcpToolCallContent, { type: "diff" }>;

/** Lines in `t`, a trailing newline ending the last line rather than starting another. */
export const lineCount = (t: string | null | undefined): number =>
  t ? t.split("\n").length - (t.endsWith("\n") ? 1 : 0) : 0;

/**
 * Where each diff starts in its file. Claude Code sends one diff per hunk, holding just that hunk
 * (leading context included), and puts each hunk's `newStart` in `locations`: the i-th diff for a
 * path pairs with the i-th location for that path. The old start follows from the earlier hunks'
 * net line change.
 *
 * A path's diffs get start lines only when it has exactly one located line per diff; any other
 * shape can't be paired safely. A whole-file diff (no old text: a created file or a Write not yet
 * narrowed to hunks; or no new text: a deleted file) is never offset, nor is a diff whose old
 * start would fall before line 1. Those start at line 1, as before.
 */
export function hunkStarts(
  diffs: Diff[],
  locations: AcpToolCall["locations"],
): ({ oldStart: number; newStart: number } | undefined)[] {
  const lines = new Map<string, number[]>();
  for (const l of locations ?? []) {
    if (typeof l.line !== "number") continue;
    const list = lines.get(l.path) ?? [];
    list.push(l.line);
    lines.set(l.path, list);
  }
  const total = new Map<string, number>();
  for (const d of diffs) total.set(d.path, (total.get(d.path) ?? 0) + 1);

  const seen = new Map<string, number>();
  const shift = new Map<string, number>();
  return diffs.map((d) => {
    const n = seen.get(d.path) ?? 0;
    seen.set(d.path, n + 1);
    const delta = shift.get(d.path) ?? 0;
    shift.set(d.path, delta + lineCount(d.newText) - lineCount(d.oldText));
    const located = lines.get(d.path) ?? [];
    if (located.length !== total.get(d.path)) return undefined;
    if (d.oldText == null || d.newText === "") return undefined;
    const line = located[n];
    if (line < 1 || line - delta < 1) return undefined;
    return { newStart: line, oldStart: line - delta };
  });
}
