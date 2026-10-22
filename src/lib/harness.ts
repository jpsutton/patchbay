/**
 * Sorting replayed user messages: real prompts, messages the agent's harness wrote as the user
 * (Claude Code's `<system-reminder>`, `<command-name>`, `<local-command-stdout>`, ...), and
 * cancel markers. Pure, so its selftest runs under plain node.
 */
import type { Entry } from "./store";

/** A complete element with a hyphenated name, starting exactly at `lastIndex`. */
const ELEMENT = /<([a-z][\w.]*-[\w.:-]*)(?:\s[^>]*)?>[\s\S]*?<\/\1\s*>/iy;
/** Claude Code's record that the user stopped a turn, replayed as a user message. */
const CANCEL = /\[Request interrupted by user[^\]\n]*\]/y;
/** Where a harness element or cancel marker could start. */
const START = /<[a-z][\w.]*-|\[Request interrupted by user/gi;

/**
 * Text made only of hyphenated XML-like elements (`<system-reminder>`, `<command-name>`,
 * `<local-command-stdout>`, ...): what Claude Code injects as the user. Hyphenated names keep
 * a prompt of pasted HTML a prompt. Returns the elements' names, or null for any other text.
 */
export function harnessTags(text: string): string[] | null {
  const tags: string[] = [];
  let i = skipSpace(text, 0);
  while (i < text.length) {
    ELEMENT.lastIndex = i;
    const m = ELEMENT.exec(text);
    if (!m) return null;
    if (!tags.includes(m[1])) tags.push(m[1]);
    i = skipSpace(text, ELEMENT.lastIndex);
  }
  return tags.length ? tags : null;
}

function skipSpace(s: string, i: number): number {
  while (i < s.length && /\s/.test(s[i])) i++;
  return i;
}

type Piece = { kind: "harness" | "cancelled"; start: number; end: number };

/** The harness element or cancel marker starting exactly at `i`, if any. */
function pieceAt(s: string, i: number): Piece | null {
  ELEMENT.lastIndex = i;
  if (ELEMENT.exec(s)) return { kind: "harness", start: i, end: ELEMENT.lastIndex };
  CANCEL.lastIndex = i;
  if (CANCEL.exec(s)) return { kind: "cancelled", start: i, end: CANCEL.lastIndex };
  return null;
}

/**
 * Splits one replayed user message (all its chunks, joined) into entries: harness elements and
 * cancel markers at its start or end become their own entries, the prompt between stays a user
 * entry. An element only comes off where it stands apart from the prompt: on its own line, or
 * in its own chunk (`bounds`: offsets where a chunk began). So "Use <my-tag>x</my-tag> here"
 * stays one prompt.
 */
export function classifyUserText(raw: string, bounds: number[] = []): Entry[] {
  // Whitespace between a and b that holds a line break or a chunk boundary.
  const apart = (a: number, b: number) =>
    a === 0 || b === raw.length || /\n/.test(raw.slice(a, b)) || bounds.some((x) => x >= a && x <= b);

  const front: Piece[] = [];
  let i = skipSpace(raw, 0);
  for (let p = pieceAt(raw, i); p; p = pieceAt(raw, i)) {
    const next = skipSpace(raw, p.end);
    if (!apart(p.end, next) && !pieceAt(raw, next)) break;
    front.push(p);
    i = next;
  }

  // The earliest start after the front from which only pieces follow, set apart from the prompt.
  let back: Piece[] = [];
  let cut = raw.length;
  START.lastIndex = i;
  for (let m = START.exec(raw); m; m = START.exec(raw)) {
    const pieces = chainFrom(raw, m.index);
    let before = m.index;
    while (before > i && /\s/.test(raw[before - 1])) before--;
    if (pieces && apart(before, m.index)) {
      back = pieces;
      cut = before;
      break;
    }
  }

  const out: Entry[] = [];
  // Adjacent elements from one chunk make one harness entry; a cancel marker is its own.
  const emit = (list: Piece[]) => {
    let from = -1;
    let to = -1;
    const flush = () => {
      if (from >= 0) out.push({ kind: "harness", text: raw.slice(from, to) });
      from = -1;
    };
    for (const p of list) {
      if (p.kind === "cancelled") {
        flush();
        out.push({ kind: "cancelled" });
        continue;
      }
      if (from >= 0 && bounds.some((x) => x >= to && x <= p.start)) flush();
      if (from < 0) from = p.start;
      to = p.end;
    }
    flush();
  };
  emit(front);
  let prompt = raw.slice(front.length ? i : 0, cut);
  if (back.length) prompt = prompt.trimEnd();
  if (prompt.trim()) out.push({ kind: "user", text: prompt });
  emit(back);
  return out;
}

/** Pieces from `i` to the end of `s` with only whitespace between them, or null. */
function chainFrom(s: string, i: number): Piece[] | null {
  const out: Piece[] = [];
  while (i < s.length) {
    const p = pieceAt(s, i);
    if (!p) return null;
    out.push(p);
    i = skipSpace(s, p.end);
  }
  return out.length ? out : null;
}

/** The replayed user message an entries array ends with: where its entries start, its text. */
const runs = new WeakMap<Entry[], { start: number; raw: string; bounds: number[] }>();

/**
 * Adds a replayed user chunk. Chunks with nothing between them are one message, classified as
 * a whole each time, so a wrapper split across chunks, or sharing a chunk with the prompt,
 * still comes out as a harness entry.
 */
export function appendUserChunk(entries: Entry[], text: string): Entry[] {
  if (!text) return entries;
  const last = entries[entries.length - 1];
  // A user entry this didn't make (a live echo) still takes the chunk, as plain text did.
  const adopt = !runs.has(entries) && last?.kind === "user" ? last : null;
  const run = runs.get(entries) ??
    (adopt ? { start: entries.length - 1, raw: adopt.text, bounds: [] } : { start: entries.length, raw: "", bounds: [] });
  const raw = run.raw + text;
  const bounds = run.raw ? [...run.bounds, run.raw.length] : run.bounds;
  const sorted = classifyUserText(raw, bounds);
  // That adopted entry keeps its attachments and prompt id.
  if (adopt) {
    const k = sorted.findIndex((e) => e.kind === "user");
    if (k >= 0) sorted[k] = { ...adopt, text: (sorted[k] as { text: string }).text };
  }
  const next = [...entries.slice(0, run.start), ...sorted];
  runs.set(next, { start: run.start, raw, bounds });
  return next;
}
