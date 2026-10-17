/** Rank paths for a fuzzy query: subsequence match, favoring basename hits, contiguous runs
 * and shorter paths. Returns at most `limit` paths. */
export function fuzzyPaths(paths: string[], query: string, limit = 50): string[] {
  const q = query.toLowerCase();
  if (!q) return paths.slice(0, limit);
  const scored: [number, string][] = [];
  for (const p of paths) {
    const s = score(p.toLowerCase(), q);
    if (s > -Infinity) scored.push([s, p]);
  }
  scored.sort((a, b) => b[0] - a[0] || a[1].length - b[1].length);
  return scored.slice(0, limit).map(([, p]) => p);
}

function score(path: string, q: string): number {
  const base = path.slice(path.lastIndexOf("/") + 1);
  if (base.startsWith(q)) return 1000 - path.length;
  if (base.includes(q)) return 800 - path.length;
  if (path.includes(q)) return 600 - path.length;
  let s = 0;
  let pi = 0;
  let run = 0;
  for (const ch of q) {
    const at = path.indexOf(ch, pi);
    if (at < 0) return -Infinity;
    run = at === pi ? run + 1 : 0;
    s += run * 2 - Math.min(at - pi, 10);
    pi = at + 1;
  }
  return s - path.length / 10;
}
