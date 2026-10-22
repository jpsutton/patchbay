import type { AcpToolCall, AcpToolCallContent } from "../lib/ipc";
import type { Entry } from "../lib/store";

/**
 * The Changes panel's view of a session: every file the agent changed, rebuilt from the `diff`
 * contents of its tool calls.
 *
 * Agents differ in what a diff holds. Codex and Gemini send whole files (Codex deletes a file
 * with `newText: ""`). Claude Code sends one diff per changed hunk, whole lines with context,
 * several for one path, each hunk's start line in the call's `locations` (its `newStart`); a
 * Write first sends `oldText: null` with the full file, replaced by hunks once it ran. So each
 * diff is a region, and regions of one file are merged only where they provably meet: by line
 * range when start lines are known and the overlap agrees, else by finding one's text in the
 * other. A diff that can't be placed is shown as its own region, never paired with another.
 */

type Diff = Extract<AcpToolCallContent, { type: "diff" }>;

/** One changed region: lines `old` became `new`. `start`: where `new` begins in the file now. */
interface Region {
  start?: number;
  old: string[];
  new: string[];
}

interface FileState {
  path: string;
  created: boolean;
  deleted: boolean;
  regions: Region[];
}

export interface ChangePart {
  /** Null when the file is new. */
  oldText: string | null;
  newText: string;
  /** First line numbers of the part on each side, for the diff's gutter. */
  oldStart: number;
  newStart: number;
}

export interface FileChange {
  /** Absolute, normalized against the session's directory. */
  path: string;
  status: "added" | "modified" | "deleted";
  /** One for a whole file; several when only separate regions of it are known. */
  parts: ChangePart[];
}

/** `p` made absolute against `cwd`, with `.`, `..`, doubled slashes and backslashes resolved. */
export function normalizePath(p: string, cwd: string): string {
  let full = p.replace(/\\/g, "/");
  if (!full.startsWith("/") && !/^[A-Za-z]:\//.test(full)) full = `${cwd.replace(/\\/g, "/")}/${full}`;
  const [root, ...rest] = full.split("/");
  const out: string[] = [];
  for (const seg of rest) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") out.pop();
    else out.push(seg);
  }
  return `${root}/${out.join("/")}`;
}

const linesOf = (t: string | null): string[] => (t ? t.split("\n") : []);

/** A region's lines as text, each line ended, so a region and a whole file diff alike. */
function textOf(lines: string[]): string {
  if (lines.length === 0) return "";
  return lines[lines.length - 1] === "" ? lines.join("\n") : lines.join("\n") + "\n";
}

const sameLines = (a: string[], b: string[]) => a.length === b.length && a.every((l, i) => l === b[i]);

/** Where `needle` occurs in `hay`: the occurrence nearest `near` when given, else the first; -1 if none. */
function findLines(hay: string[], needle: string[], near?: number): number {
  if (needle.length === 0 || needle.length > hay.length) return -1;
  let best = -1;
  for (let i = 0; i + needle.length <= hay.length; i++) {
    let k = 0;
    while (k < needle.length && hay[i + k] === needle[k]) k++;
    if (k < needle.length) continue;
    if (near === undefined) return i;
    if (best < 0 || Math.abs(i - near) < Math.abs(best - near)) best = i;
  }
  return best;
}

/** Moves the regions that start at or after `from` by `by` lines. */
function shift(regions: Region[], from: number, by: number, except?: Region) {
  if (by === 0) return;
  for (const r of regions) if (r !== except && r.start !== undefined && r.start >= from) r.start += by;
}

/** Whether line ranges [a0, a1) and [b0, b1) share a line; an empty range is a point between lines. */
function overlaps(a0: number, a1: number, b0: number, b1: number): boolean {
  if (a0 === a1) return b0 < a0 && a0 < b1;
  if (b0 === b1) return a0 < b0 && b0 < a1;
  return a0 < b1 && b0 < a1;
}

/**
 * Applies a diff at known line `h` by line arithmetic, merging the regions it overlaps. False
 * when the overlapping lines disagree, so the caller doesn't merge.
 */
function mergeAt(f: FileState, h: number, oldL: string[], newL: string[]): boolean {
  const end = h + oldL.length;
  const hit = f.regions.filter((r) => overlaps(r.start!, r.start! + r.new.length, h, end));
  if (hit.length === 0) {
    shift(f.regions, end, newL.length - oldL.length);
    f.regions.push({ start: h, old: oldL, new: newL });
    return true;
  }
  const u0 = Math.min(h, ...hit.map((r) => r.start!));
  const u1 = Math.max(end, ...hit.map((r) => r.start! + r.new.length));
  // The file's current lines over the merged range: from the regions, checked against the diff.
  const cur: (string | undefined)[] = new Array(u1 - u0);
  for (const r of hit) r.new.forEach((l, k) => (cur[r.start! - u0 + k] = l));
  for (let k = 0; k < oldL.length; k++) {
    const i = h - u0 + k;
    if (cur[i] !== undefined && cur[i] !== oldL[k]) return false;
    cur[i] = oldL[k];
  }
  if (cur.some((l) => l === undefined)) return false;
  const old: string[] = [];
  let pos = u0;
  for (const r of [...hit].sort((a, b) => a.start! - b.start!)) {
    while (pos < r.start!) old.push(cur[pos++ - u0]!);
    old.push(...r.old);
    pos = r.start! + r.new.length;
  }
  while (pos < u1) old.push(cur[pos++ - u0]!);
  const now = [...cur.slice(0, h - u0), ...newL, ...cur.slice(end - u0)] as string[];
  f.regions = f.regions.filter((r) => !hit.includes(r));
  shift(f.regions, u1, newL.length - oldL.length);
  f.regions.push({ start: u0, old, new: now });
  return true;
}

/** Applies one diff to a file's regions. `hint`: the line the diff starts at now, when known. */
function applyDiff(f: FileState, d: Diff, hint: number | undefined) {
  const oldL = linesOf(d.oldText);
  const newL = linesOf(d.newText);
  const delta = newL.length - oldL.length;
  f.deleted = false;
  if (f.regions.length === 0) {
    f.created = d.oldText === null;
    f.regions.push({ start: hint ?? (f.created ? 1 : undefined), old: oldL, new: newL });
  } else if (d.oldText === null && hint === undefined) {
    // A whole new text for a file seen before (a Write not yet narrowed to hunks).
    const whole = f.created && f.regions.length === 1 ? f.regions[0] : null;
    if (whole) whole.new = newL;
    else f.regions.push({ old: [], new: newL });
  } else if (!(hint !== undefined && f.regions.every((r) => r.start !== undefined) && mergeAt(f, hint, oldL, newL))) {
    applyByText(f, oldL, newL, delta, hint);
  }
  // Codex deletes a file by sending all of it as removed.
  f.deleted =
    d.newText === "" && hint === undefined && oldL.length > 0 && f.regions.length === 1 && f.regions[0].new.length === 0;
}

/** Places a diff by its text: inside a region's current lines, or around one. Else on its own. */
function applyByText(f: FileState, oldL: string[], newL: string[], delta: number, hint: number | undefined) {
  for (const r of f.regions) {
    const near = hint === undefined ? undefined : hint - (r.start ?? 1);
    const at = findLines(r.new, oldL, near);
    if (at < 0) continue;
    r.new = [...r.new.slice(0, at), ...newL, ...r.new.slice(at + oldL.length)];
    if (r.start !== undefined) shift(f.regions, r.start + 1, delta, r);
    return;
  }
  // The diff's wider context holds a region whole: the merged region is the diff's, with the
  // region's original lines in place of its current ones.
  for (const r of f.regions) {
    const at = findLines(oldL, r.new);
    if (at < 0 || (hint !== undefined && r.start !== undefined && hint + at !== r.start)) continue;
    r.old = [...oldL.slice(0, at), ...r.old, ...oldL.slice(at + r.new.length)];
    r.new = newL;
    if (r.start !== undefined) {
      r.start -= at;
      shift(f.regions, r.start + 1, delta, r);
    } else r.start = hint;
    return;
  }
  if (hint !== undefined) shift(f.regions, hint + oldL.length, delta);
  f.regions.push({ start: hint, old: oldL, new: newL });
}

/** Every file the session's tool calls changed, in first-touched order. Failed calls changed nothing. */
export function collectChanges(order: Entry[], toolCalls: Record<string, AcpToolCall>, cwd: string): FileChange[] {
  const files = new Map<string, FileState>();
  for (const e of order) {
    if (e.kind !== "tool") continue;
    const tc = toolCalls[e.id];
    if (!tc || tc.status === "failed") continue;
    const diffs = (tc.content ?? []).filter((c): c is Diff => c.type === "diff");
    if (diffs.length === 0) continue;
    // Claude sends one location per diff, the hunk's start line; trust lines only in that shape.
    const locs = tc.locations ?? [];
    const lined =
      locs.length === diffs.length &&
      locs.every((l, i) => typeof l.line === "number" && l.line > 0 && normalizePath(l.path, cwd) === normalizePath(diffs[i].path, cwd));
    diffs.forEach((d, i) => {
      const path = normalizePath(d.path, cwd);
      let f = files.get(path);
      if (!f) files.set(path, (f = { path, created: false, deleted: false, regions: [] }));
      applyDiff(f, d, lined ? locs[i].line! : undefined);
    });
  }
  const out: FileChange[] = [];
  for (const f of files.values()) {
    if (f.created && f.deleted) continue;
    const regions = f.regions
      .filter((r) => !sameLines(r.old, r.new))
      .sort((a, b) => (a.start ?? Infinity) - (b.start ?? Infinity));
    if (regions.length === 0) continue;
    const known = regions.every((r) => r.start !== undefined);
    let shiftBy = 0;
    const parts = regions.map((r): ChangePart => {
      const newStart = known ? r.start! : 1;
      const part = {
        oldText: f.created && regions.length === 1 && r.old.length === 0 ? null : textOf(r.old),
        newText: textOf(r.new),
        oldStart: newStart - shiftBy,
        newStart,
      };
      if (known) shiftBy += r.new.length - r.old.length;
      return part;
    });
    out.push({ path: f.path, status: f.deleted ? "deleted" : f.created ? "added" : "modified", parts });
  }
  return out;
}
