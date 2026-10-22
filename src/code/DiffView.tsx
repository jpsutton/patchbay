import { useMemo } from "react";
import { structuredPatch } from "diff";

export function diffStats(oldText: string | null, newText: string): { add: number; del: number } {
  const p = structuredPatch("a", "b", oldText ?? "", newText, "", "", { context: 0 });
  let add = 0;
  let del = 0;
  for (const h of p.hunks)
    for (const l of h.lines) {
      if (l.startsWith("+")) add++;
      else if (l.startsWith("-")) del++;
    }
  return { add, del };
}

export function DiffView({
  path,
  oldText,
  newText,
  maxHeight = 420,
  oldStart = 1,
  newStart = 1,
}: {
  path: string;
  oldText: string | null;
  newText: string;
  maxHeight?: number;
  /** The file's line numbers where the texts begin, when they're a region of it; null: unknown. */
  oldStart?: number | null;
  newStart?: number | null;
}) {
  const patch = useMemo(
    () => structuredPatch(path, path, oldText ?? "", newText, "", "", { context: 3 }),
    [path, oldText, newText],
  );
  const isNew = oldText === null || oldText === "";
  return (
    <div className="overflow-hidden rounded-xl border border-line font-mono text-[12.5px] leading-5">
      <div className="flex h-8 items-center gap-2 border-b border-line bg-panel px-3 text-[12px]">
        <span className="truncate text-fg/90">{path}</span>
        {isNew && <span className="rounded-[5px] bg-ok/15 px-1.5 text-[11px] text-ok">new</span>}
      </div>
      <div className="overflow-auto" style={{ maxHeight }}>
        {patch.hunks.length === 0 && <div className="px-3 py-2 text-faint">No changes</div>}
        {patch.hunks.map((h, hi) => {
          // Unknown starts show no line numbers rather than made-up ones.
          const known = oldStart !== null && newStart !== null;
          let oldNo = h.oldStart + (oldStart ?? 1) - 1;
          let newNo = h.newStart + (newStart ?? 1) - 1;
          return (
            <div key={hi}>
              <div className="bg-panel px-3 py-1 text-faint">
                {known ? `@@ -${oldNo},${h.oldLines} +${newNo},${h.newLines} @@` : `@@ -${h.oldLines} +${h.newLines} lines, position unknown @@`}
              </div>
              {h.lines.map((l, li) => {
                if (l.startsWith("\\")) return null;
                const sign = l[0];
                const body = l.slice(1);
                let a: number | string = "";
                let b: number | string = "";
                if (sign === "-") a = oldNo++;
                else if (sign === "+") b = newNo++;
                else {
                  a = oldNo++;
                  b = newNo++;
                }
                if (!known) a = b = "";
                const bg = sign === "+" ? "bg-add-bg" : sign === "-" ? "bg-del-bg" : "";
                const tone = sign === "+" ? "text-ok" : sign === "-" ? "text-danger" : "text-faint";
                return (
                  <div key={li} className={`flex whitespace-pre ${bg}`}>
                    <span className="w-10 shrink-0 select-none pr-2 text-right text-faint/60">{a}</span>
                    <span className="w-10 shrink-0 select-none pr-2 text-right text-faint/60">{b}</span>
                    <span className={`w-4 shrink-0 select-none ${tone}`}>{sign === " " ? "" : sign}</span>
                    <span className={`pr-3 ${sign === " " ? "text-fg/70" : "text-fg"}`}>{body}</span>
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
}
