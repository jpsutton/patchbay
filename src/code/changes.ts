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
 * other, and only where that text occurs once. A diff that can't be placed is shown as its own
 * region, never paired with another, and without line numbers. Lines compare without a trailing
 * `\r`, so LF diffs meet CRLF files; the text shown is the agent's own.
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
  /**
   * The call whose `oldText: null` diff made the file count as created, while that's the last
   * word on it. A Write over an existing file sends one before its real hunks.
   */
  nullCall?: string;
}

export interface ChangePart {
  /** Null when the file is new. */
  oldText: string | null;
  newText: string;
  /** First line numbers of the part on each side, for the diff's gutter; null when unknown. */
  oldStart: number | null;
  newStart: number | null;
}

export interface FileChange {
  /** Absolute, normalized against the session's directory. */
  path: string;
  status: "added" | "modified" | "deleted";
  /** One for a whole file; several when only separate regions of it are known. */
  parts: ChangePart[];
}

/**
 * `p` made absolute against `cwd`, with `.`, `..`, doubled slashes and backslashes resolved. A
 * Windows drive letter is uppercased, so `c:` and `C:` meet; a UNC `\\server\share` keeps its
 * leading pair, apart from POSIX paths.
 */
export function normalizePath(p: string, cwd: string): string {
  let full = p.replace(/\\/g, "/");
  if (!full.startsWith("/") && !/^[A-Za-z]:\//.test(full)) full = `${cwd.replace(/\\/g, "/").replace(/\/+$/, "")}/${full}`;
  let prefix = "/";
  let floor = 0;
  if (/^\/\/[^/]/.test(full)) {
    // UNC: `..` can't climb above the share.
    prefix = "//";
    floor = 2;
    full = full.slice(2);
  } else if (/^[A-Za-z]:/.test(full)) {
    prefix = `${full[0].toUpperCase()}:/`;
    full = full.slice(2);
  }
  const out: string[] = [];
  for (const seg of full.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (out.length > floor) out.pop();
    } else out.push(seg);
  }
  return prefix + out.join("/");
}

const linesOf = (t: string | null): string[] => (t ? t.split("\n") : []);

/** A region's lines as text, each line ended, so a region and a whole file diff alike. */
function textOf(lines: string[]): string {
  if (lines.length === 0) return "";
  return lines[lines.length - 1] === "" ? lines.join("\n") : lines.join("\n") + "\n";
}

/** Lines are equal whether or not they end in `\r`: a CRLF file meets an LF diff. */
const sameLine = (a: string | undefined, b: string | undefined) =>
  a === b || (a !== undefined && b !== undefined && a.replace(/\r$/, "") === b.replace(/\r$/, ""));

const sameLines = (a: string[], b: string[]) => a.length === b.length && a.every((l, i) => sameLine(l, b[i]));

/** Every index where `needle` occurs in `hay`. An empty needle occurs nowhere. */
function findAll(hay: string[], needle: string[]): number[] {
  const out: number[] = [];
  if (needle.length === 0 || needle.length > hay.length) return out;
  for (let i = 0; i + needle.length <= hay.length; i++) {
    let k = 0;
    while (k < needle.length && sameLine(hay[i + k], needle[k])) k++;
    if (k === needle.length) out.push(i);
  }
  return out;
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
    if (cur[i] !== undefined && !sameLine(cur[i], oldL[k])) return false;
    cur[i] ??= oldL[k];
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

/**
 * Applies one diff to a file's regions. `hint`: the line the diff starts at now, when known.
 * `call`: the tool call it came from.
 */
function applyDiff(f: FileState, d: Diff, hint: number | undefined, call: string) {
  const oldL = linesOf(d.oldText);
  const newL = linesOf(d.newText);
  // A diff with old text after a Write's `oldText: null` one: the Write's own hunks (same call),
  // or a change that isn't in the text it wrote, mean the file existed; the null diff was only
  // the Write's preview. An edit inside the written text keeps the file new.
  if (f.nullCall !== undefined && d.oldText !== null) {
    const whole = f.regions[0];
    if (f.nullCall === call || findAll(whole.new, oldL).length === 0) {
      f.created = false;
      f.regions = [];
    }
    f.nullCall = undefined;
  }
  f.deleted = false;
  if (f.regions.length === 0) {
    f.created = d.oldText === null;
    if (f.created) f.nullCall = call;
    f.regions.push({ start: hint ?? (f.created ? 1 : undefined), old: oldL, new: newL });
  } else if (d.oldText === null && hint === undefined) {
    // A whole new text for a file seen before (a Write not yet narrowed to hunks).
    const whole = f.created && f.regions.length === 1 ? f.regions[0] : null;
    if (whole) {
      whole.new = newL;
      f.nullCall = call;
    } else f.regions.push({ old: [], new: newL });
  } else if (!(hint !== undefined && f.regions.every((r) => r.start !== undefined) && mergeAt(f, hint, oldL, newL))) {
    applyByText(f, oldL, newL, hint);
  }
  // Codex deletes a file by sending all of it as removed: a whole-file diff (no line) to "".
  if (d.newText === "" && d.oldText !== null && oldL.length > 0 && hint === undefined) {
    f.deleted = true;
    f.nullCall = undefined;
    // What was there when it went, unless the earlier edits merged into one removal.
    if (!(f.regions.length === 1 && f.regions[0].new.length === 0)) f.regions = [{ start: 1, old: oldL, new: [] }];
  }
}

/**
 * Places a diff by its text: inside a region's current lines, or around regions. With no line
 * hint only a text that occurs once is placed. Else the diff is a region on its own.
 */
function applyByText(f: FileState, oldL: string[], newL: string[], hint: number | undefined) {
  const delta = newL.length - oldL.length;
  // Inside one region: the occurrence nearest the hint, or the only one.
  const hits: { r: Region; at: number }[] = [];
  for (const r of f.regions) for (const at of findAll(r.new, oldL)) hits.push({ r, at });
  let pick: { r: Region; at: number } | undefined;
  if (hint === undefined) pick = hits.length === 1 ? hits[0] : undefined;
  else {
    const dist = (x: { r: Region; at: number }) => Math.abs((x.r.start ?? 1) + x.at - hint);
    pick = hits.reduce<typeof pick>((best, x) => (!best || dist(x) < dist(best) ? x : best), undefined);
  }
  if (pick) {
    const { r, at } = pick;
    r.new = [...r.new.slice(0, at), ...newL, ...r.new.slice(at + oldL.length)];
    if (r.start !== undefined) shift(f.regions, r.start + 1, delta, r);
    return;
  }
  // The diff's wider context holds regions whole: the merged region is the diff's, with each
  // region's original lines in place of its current ones. Every such region merges, or none.
  const inside: { r: Region; at: number }[] = [];
  for (const r of f.regions) {
    const found = findAll(oldL, r.new);
    const at =
      hint !== undefined && r.start !== undefined
        ? found.find((p) => hint + p === r.start)
        : found.length === 1
          ? found[0]
          : undefined;
    if (at !== undefined) inside.push({ r, at });
  }
  inside.sort((a, b) => a.at - b.at);
  const apart = inside.every((x, i) => i === 0 || x.at >= inside[i - 1].at + inside[i - 1].r.new.length);
  const starts = new Set(inside.filter((x) => x.r.start !== undefined).map((x) => x.r.start! - x.at));
  if (inside.length > 0 && apart && starts.size <= 1) {
    const old: string[] = [];
    let pos = 0;
    for (const { r, at } of inside) {
      old.push(...oldL.slice(pos, at), ...r.old);
      pos = at + r.new.length;
    }
    old.push(...oldL.slice(pos));
    const start = hint ?? [...starts][0];
    f.regions = f.regions.filter((r) => !inside.some((x) => x.r === r));
    if (start !== undefined) shift(f.regions, start + 1, delta);
    f.regions.push({ start, old, new: newL });
    return;
  }
  if (hint !== undefined) shift(f.regions, hint + oldL.length, delta);
  f.regions.push({ start: hint, old: oldL, new: newL });
}

/**
 * What `collectChanges` reads of a session: the diff-bearing tool calls' contents, statuses and
 * locations, in order. Equal keys (element by element) give equal changes.
 */
export function changesKey(order: Entry[], toolCalls: Record<string, AcpToolCall>): unknown[] {
  const key: unknown[] = [];
  for (const e of order) {
    if (e.kind !== "tool") continue;
    const tc = toolCalls[e.id];
    if (!tc?.content?.some((c) => c.type === "diff")) continue;
    key.push(e.id, tc.content, tc.status === "failed", tc.locations);
  }
  return key;
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
      applyDiff(f, d, lined ? locs[i].line! : undefined, e.id);
    });
  }
  const out: FileChange[] = [];
  for (const f of files.values()) {
    if (f.created && f.deleted) continue;
    const regions = f.regions
      .filter((r) => !sameLines(r.old, r.new))
      .sort((a, b) => (a.start ?? Infinity) - (b.start ?? Infinity));
    if (regions.length === 0) continue;
    // Line numbers only when every region is placed. A lone region with no line is a whole file
    // (Codex and Gemini send those), so it starts at line 1.
    const known = regions.length === 1 || regions.every((r) => r.start !== undefined);
    let shiftBy = 0;
    const parts = regions.map((r): ChangePart => {
      const newStart = known ? (r.start ?? 1) : null;
      const part = {
        oldText: f.created && regions.length === 1 && r.old.length === 0 ? null : textOf(r.old),
        newText: textOf(r.new),
        oldStart: newStart === null ? null : newStart - shiftBy,
        newStart,
      };
      shiftBy += r.new.length - r.old.length;
      return part;
    });
    out.push({ path: f.path, status: f.deleted ? "deleted" : f.created ? "added" : "modified", parts });
  }
  return out;
}
