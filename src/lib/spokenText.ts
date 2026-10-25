/**
 * What Read aloud says. The same words on every surface: prose, short code, and a stand-in
 * for a long fence or a diagram. Reasoning never reaches this function; callers pass the reply.
 */

const SHORT_CODE_LINES = 8;

export function spokenText(markdown: string): string {
  let s = markdown.replace(/\r\n/g, "\n");
  s = s.replace(/```([^\n]*)\n?([\s\S]*?)```/g, (_m, lang: string, body: string) => fence(lang, body));
  s = s.replace(/`([^`\n]+)`/g, "$1");
  s = s.replace(/!\[[^\]]*]\([^)]*\)/g, " ");
  s = s.replace(/\[([^\]]+)]\([^)]*\)/g, "$1");
  s = s
    .split("\n")
    .filter((line) => !/^\s*\|.*\|\s*$/.test(line))
    .join("\n");
  s = s.replace(/^#{1,6}\s+/gm, "");
  s = s.replace(/[*_~>]+/g, "");
  s = s.replace(/<[^>\n]+>/g, " ");
  s = s.replace(/[ \t]+/g, " ");
  s = s.replace(/\n{2,}/g, "\n");
  return s
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .join("\n")
    .trim();
}

function fence(lang: string, body: string): string {
  if (/mermaid/i.test(lang)) return " Diagram. ";
  const lines = body
    .replace(/\s+$/, "")
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  if (lines.length === 0) return " ";
  if (lines.length > SHORT_CODE_LINES || lines.some((l) => l.startsWith("+") || l.startsWith("-") || l.startsWith("diff "))) {
    return " Code block. ";
  }
  return ` Code: ${lines.join(", ")}. `;
}

/** Characters in the first piece, so speech starts quickly however long the reply is. */
const FIRST_CHUNK = 160;

/**
 * Sentence-sized pieces so Stop lands between them. Pieces grow: the first holds about
 * `first` characters and each later one is twice as long as the one before, up to `max`.
 * A short first piece means audio starts early. A longer later piece means fewer joins, and
 * the next piece can be made while the current one plays.
 */
export function speechChunks(text: string, max = 320, first = FIRST_CHUNK): string[] {
  const sentences: string[] = [];
  let cur = "";
  for (const ch of text) {
    cur += ch;
    if (ch === "\n" || ch === "." || ch === "!" || ch === "?") {
      const piece = cur.trim();
      if (piece) sentences.push(piece);
      cur = "";
    }
  }
  const tail = cur.trim();
  if (tail) sentences.push(tail);

  const out: string[] = [];
  let limit = Math.min(max, Math.max(1, first));
  const grow = () => {
    limit = Math.min(max, limit * 2);
  };
  const push = (piece: string) => {
    if (piece.length <= max) {
      out.push(piece);
      return;
    }
    for (let i = 0; i < piece.length; i += max) out.push(piece.slice(i, i + max));
  };
  let buf = "";
  for (const sentence of sentences) {
    const next = buf ? `${buf} ${sentence}` : sentence;
    if (next.length > limit && buf) {
      push(buf);
      grow();
      buf = sentence;
    } else {
      buf = next;
    }
  }
  if (buf) push(buf);
  return out;
}

/** Characters in the first segment sent to the rewriting model, so speech starts quickly. */
const FIRST_SEGMENT = 500;
const MAX_SEGMENT = 3000;

/**
 * Cut markdown into parts for the rewriting model, at blank lines and never inside a code
 * fence. The first part is short so audio starts early; later parts double up to a cap, so
 * the model makes the next part while the current one plays. Joined, the parts are the input.
 */
export function speechSegments(markdown: string, first = FIRST_SEGMENT, max = MAX_SEGMENT): string[] {
  const blocks: string[] = [];
  let cur: string[] = [];
  let fenced = false;
  const flush = () => {
    if (cur.length) blocks.push(cur.join("\n"));
    cur = [];
  };
  for (const line of markdown.replace(/\r\n/g, "\n").split("\n")) {
    if (/^\s*(```|~~~)/.test(line)) fenced = !fenced;
    if (!fenced && line.trim() === "") flush();
    else cur.push(line);
  }
  flush();

  const out: string[] = [];
  let limit = Math.min(max, Math.max(1, first));
  let buf = "";
  for (const block of blocks) {
    const next = buf ? `${buf}\n\n${block}` : block;
    if (next.length > limit && buf) {
      out.push(buf);
      limit = Math.min(max, limit * 2);
      buf = block;
    } else {
      buf = next;
    }
  }
  if (buf) out.push(buf);
  return out;
}

/** Agent prose written after the latest user message in a Code session. */
export function proseSinceLastUser(entries: { kind: string; text?: string }[]): string {
  let start = 0;
  entries.forEach((e, i) => {
    if (e.kind === "user") start = i + 1;
  });
  return entries
    .slice(start)
    .filter((e) => e.kind === "agent" && e.text)
    .map((e) => e.text as string)
    .join("\n\n");
}

/** Where live reading of a Code session has got to. */
export interface LiveCursor {
  /** Index of the first entry not fully spoken. */
  next: number;
  /** Characters of that entry already spoken. */
  spoken: number;
}

type LiveEntry = { kind: string; text?: string };

/** Start at what is on screen now: the message being written, if any, and everything after. */
export function liveCursorNow(entries: LiveEntry[]): LiveCursor {
  const last = entries[entries.length - 1];
  return { next: last?.kind === "agent" ? entries.length - 1 : entries.length, spoken: 0 };
}

/**
 * The agent prose that can be spoken now. A message is done once another entry follows it.
 * The last one is done only when `settled` (the turn is idle). Text added to a spoken last
 * entry later is picked up by the next call, so nothing is read twice or dropped.
 */
export function takeLiveProse(
  entries: LiveEntry[],
  from: LiveCursor,
  settled: boolean,
): { texts: string[]; cursor: LiveCursor } {
  let next = from.next;
  let spoken = from.spoken;
  if (next > entries.length) {
    next = entries.length;
    spoken = 0;
  }
  const texts: string[] = [];
  for (let i = next; i < entries.length; i++) {
    const e = entries[i];
    const last = i === entries.length - 1;
    if (e.kind !== "agent" || !e.text) {
      next = i + 1;
      spoken = 0;
      continue;
    }
    if (last && !settled) break;
    const piece = e.text.slice(Math.min(spoken, e.text.length));
    if (piece.trim()) texts.push(piece);
    if (last) {
      next = i;
      spoken = e.text.length;
    } else {
      next = i + 1;
      spoken = 0;
    }
  }
  return { texts, cursor: { next, spoken } };
}
