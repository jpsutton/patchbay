// Models write math as $…$, $$…$$, \(…\) or \[…\]. remark-math only knows dollars, and a lone
// "$" is too often a price, so normalize everything to double dollars (inline `$$x$$` is still
// inline math) and leave single dollars to the pandoc rule: no space just inside either
// delimiter, and no digit right after the closing one. "$5 and $10" stays text.

const FENCE = /^\s*(`{3,}|~{3,})/;
const INLINE_CODE = /(`+)[^`]*?\1/g;

const SINGLE_DOLLAR = /(^|[^\\$])\$(?!\$)(\S(?:[^$\n]*?\S)?)\$(?![\d$])/g;

function normalizeProse(s: string): string {
  return s
    .replace(/\\\[([\s\S]+?)\\\]/g, (_m, body: string) => `\n$$\n${body.trim()}\n$$\n`)
    .replace(/\\\(([\s\S]+?)\\\)/g, (_m, body: string) => `$$${body.trim()}$$`)
    .replace(SINGLE_DOLLAR, (_m, pre: string, body: string) => `${pre}$$${body}$$`);
}

/** Apply `normalizeProse` outside inline code spans. */
function normalizeChunk(chunk: string): string {
  let out = "";
  let last = 0;
  for (const m of chunk.matchAll(INLINE_CODE)) {
    out += normalizeProse(chunk.slice(last, m.index)) + m[0];
    last = m.index! + m[0].length;
  }
  return out + normalizeProse(chunk.slice(last));
}

export function normalizeMath(text: string): string {
  if (!/[$\\]/.test(text)) return text;
  const lines = text.split("\n");
  const out: string[] = [];
  let prose: string[] = [];
  let fence: string | null = null;
  const flush = () => {
    if (prose.length) out.push(normalizeChunk(prose.join("\n")));
    prose = [];
  };
  for (const line of lines) {
    const f = FENCE.exec(line)?.[1];
    if (fence) {
      out.push(line);
      if (f && f[0] === fence[0] && f.length >= fence.length) fence = null;
    } else if (f) {
      flush();
      fence = f;
      out.push(line);
    } else {
      prose.push(line);
    }
  }
  flush();
  return out.join("\n");
}
