// What a narrow-pane layout measurement must satisfy (pb-20261110-10). Pure: no browser, so the
// rules can be tested in `pnpm run selftests` and a failed or empty measurement can never read as
// a pass. `scripts/e2e-layout-measure.mjs` measures; this decides.
export const WIDTHS = [180, 250, 340, 500];
export const PANES = 4;

const isNum = (n) => typeof n === "number" && Number.isFinite(n);

/** Every way the measurement is wrong or missing, as readable lines. Empty list = it passed. */
export function layoutProblems(result) {
  const out = [];
  const bad = (msg) => out.push(msg);
  if (!result || typeof result !== "object" || !result.panes || typeof result.panes !== "object") {
    return ["the measurement is empty: no `panes` in the result"];
  }
  for (const width of WIDTHS) {
    const panes = result.panes[width];
    if (!Array.isArray(panes) || panes.length === 0) {
      bad(`${width}px: no panes were measured`);
      continue;
    }
    if (panes.length !== PANES) bad(`${width}px: measured ${panes.length} panes, expected ${PANES}`);
    panes.forEach((p, i) => {
      const at = `${width}px pane #${i + 1}`;
      if (!p || typeof p !== "object") return bad(`${at}: no record`);
      // A probe that found nothing is a failed probe, never a clean one.
      if (!isNum(p.paneWidth) || Math.abs(p.paneWidth - width) > 2) bad(`${at}: pane width ${p.paneWidth} is not ${width}`);
      if (!Array.isArray(p.pastPaneEdge)) bad(`${at}: the past-the-edge probe returned nothing`);
      else if (p.pastPaneEdge.length) bad(`${at}: ${p.pastPaneEdge.length} element(s) past the pane edge: ${p.pastPaneEdge.slice(0, 4).join(", ")}`);
      if (!p.usage || typeof p.usage.shown !== "string" || !p.usage.shown) bad(`${at}: the usage line was not found`);
      else if (!isNum(p.usage.totalsOverlapRingPx)) bad(`${at}: the usage ring was not found`);
      else if (p.usage.totalsOverlapRingPx > 0) bad(`${at}: usage totals overlap the ring by ${p.usage.totalsOverlapRingPx}px`);
      if (!p.textarea) bad(`${at}: the composer textarea was not found`);
      else if (p.textarea.clipped) bad(`${at}: the composer text is clipped (${p.textarea.clientHeight}/${p.textarea.scrollHeight})`);
      // The injected state must be on screen, or the checks above ran on nothing.
      if (i === 0) {
        if (!p.queuedRow) bad(`${at}: the injected queued-message row was not found`);
        else if (!isNum(p.queuedRow.rowRightMinusPaneRight)) bad(`${at}: the queued row has no measurement`);
        else if (p.queuedRow.rowRightMinusPaneRight > 0) bad(`${at}: the queued row ends ${p.queuedRow.rowRightMinusPaneRight}px past the pane`);
      }
      if ((i === 0 || i === 3) && !(Array.isArray(p.workingLine) && p.workingLine.length)) bad(`${at}: the working line was not found`);
    });
  }
  return out;
}

/** Throws with every problem when the measurement fails; returns when it passes. */
export function assertLayout(result) {
  const problems = layoutProblems(result);
  if (problems.length) throw new Error(`layout measurement FAILED (${problems.length}):\n  - ${problems.join("\n  - ")}`);
}
