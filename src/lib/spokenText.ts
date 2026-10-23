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

/** Sentence-sized pieces so Stop lands between them. */
export function speechChunks(text: string, max = 320): string[] {
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
  let buf = "";
  const push = (piece: string) => {
    if (piece.length <= max) {
      out.push(piece);
      return;
    }
    for (let i = 0; i < piece.length; i += max) out.push(piece.slice(i, i + max));
  };
  for (const sentence of sentences) {
    const next = buf ? `${buf} ${sentence}` : sentence;
    if (next.length > max && buf) {
      push(buf);
      buf = sentence;
    } else {
      buf = next;
    }
  }
  if (buf) push(buf);
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
