// What a narrow-pane layout measurement must satisfy (pb-20261110-10). Pure: no browser, so the
// rules can be tested in `pnpm run selftests` and a failed or empty measurement can never read as
// a pass. `scripts/e2e-layout-measure.mjs` measures; this decides.
export const WIDTHS = [180, 250, 340, 500];
export const PANES = 4;
/** Widths of a single un-split pane, where the header holds the mode and approval controls itself. */
export const SINGLE_WIDTHS = [900, 720, 600, 480];

const isNum = (n) => typeof n === "number" && Number.isFinite(n);

/**
 * The usage line drops its cost under 14rem of ITS OWN width (a container query), not the pane's.
 * Measured on the clean tree: panes of 180 and 250 px show no "$", panes of 340 and 500 px show it,
 * so the cut sits between (a pane of about 300 px is a usage row of about 14rem).
 */
const COST_SHOWN_FROM = 300;

/** A header control narrower than this is squeezed out of use (the shared chip's floor is 56px; without it the chip is 2px). */
const MIN_CONTROL_PX = 40;

/** The widest pane for which a header past its edge is not asserted (180px: the Approvals control, 232px wide, is wider than the pane). */
const STRICT_SPLIT_FROM = 250;
/** A lone pane narrower than this overflows its header with or without the chip's floor (measured). */
const STRICT_SINGLE_FROM = 720;

/** The header's mode and approval controls in one pane. `strict`: also no overflow and nothing past the pane edge. */
function headerProblems(p, at, strict, bad) {
  const h = p?.headerControls;
  if (!h || typeof h !== "object") return bad(`${at}: the header was not found`);
  for (const name of ["mode", "approval"]) {
    const c = h[name];
    if (!c) bad(`${at}: the ${name} control was not found in the header`);
    else if (!isNum(c.width) || c.width <= 0) bad(`${at}: the ${name} control has no width`);
    else if (!isNum(c.pastPaneEdgePx)) bad(`${at}: the ${name} control has no position`);
    else if (c.width < MIN_CONTROL_PX) bad(`${at}: the ${name} control is squeezed to ${c.width}px`);
    else if (strict && c.pastPaneEdgePx > 0.5) bad(`${at}: the ${name} control ends ${c.pastPaneEdgePx}px past the pane edge`);
  }
  if (!Array.isArray(h.collisions)) bad(`${at}: the header collision probe returned nothing`);
  else if (h.collisions.length) bad(`${at}: header controls overlap each other (${h.collisions.join(", ")})`);
  if (strict && h.headerOverflow) bad(`${at}: the header overflows`);
  if (strict && h.subheaderOverflow) bad(`${at}: the sub-header overflows`);
}

/** The chip's minimum width must never make the header worse than the same pane without it. */
function floorProblems(on, off, at, bad) {
  const a = on?.headerControls;
  const b = off?.headerControls;
  if (!b || typeof b !== "object") return bad(`${at}: the measurement without the chip's floor is missing`);
  if (!a || typeof a !== "object") return;
  if (a.headerOverflow && !b.headerOverflow) bad(`${at}: the chip's floor makes the header overflow`);
  if (a.subheaderOverflow && !b.subheaderOverflow) bad(`${at}: the chip's floor makes the sub-header overflow`);
  for (const name of ["mode", "approval"]) {
    const x = a[name]?.pastPaneEdgePx;
    const y = b[name]?.pastPaneEdgePx;
    if (isNum(x) && isNum(y) && x > 0.5 && x > y + 0.5) bad(`${at}: the chip's floor pushes the ${name} control ${Math.round((x - Math.max(y, 0)) * 10) / 10}px further past the pane edge`);
  }
  if (Array.isArray(a.collisions) && Array.isArray(b.collisions) && a.collisions.length > b.collisions.length) bad(`${at}: the chip's floor makes header controls overlap`);
}

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
      headerProblems(p, at, width >= STRICT_SPLIT_FROM, bad);
      floorProblems(p, result.noFloor?.[width]?.[i], at, bad);
      // A probe that found nothing is a failed probe, never a clean one.
      if (!isNum(p.paneWidth) || Math.abs(p.paneWidth - width) > 2) bad(`${at}: pane width ${p.paneWidth} is not ${width}`);
      if (!Array.isArray(p.pastPaneEdge)) bad(`${at}: the past-the-edge probe returned nothing`);
      else if (p.pastPaneEdge.length) bad(`${at}: ${p.pastPaneEdge.length} element(s) past the pane edge: ${p.pastPaneEdge.slice(0, 4).join(", ")}`);
      if (!p.usage || typeof p.usage.shown !== "string" || !p.usage.shown) bad(`${at}: the usage line was not found`);
      else if (width < COST_SHOWN_FROM && p.usage.shown.includes("$")) bad(`${at}: the usage line shows its cost ("${p.usage.shown}") in a pane this narrow`);
      else if (width >= COST_SHOWN_FROM && !p.usage.shown.includes("$")) bad(`${at}: the usage line lost its cost ("${p.usage.shown}")`);
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
      // The first span is the label ("Writing…"); it keeps its width. The timer after it is meant to
      // truncate in narrow panes (it does in the clean run), so only the label is asserted.
      else if ((i === 0 || i === 3) && /\| TRUNCATED \|/.test(p.workingLine[0])) bad(`${at}: the working-line label is truncated (${p.workingLine[0]})`);
    });
  }
  // One pane on its own: the header row holds the controls itself.
  for (const width of SINGLE_WIDTHS) {
    const panes = result.single?.[width];
    if (!Array.isArray(panes) || panes.length !== 1) {
      bad(`single pane at ${width}px: ${Array.isArray(panes) ? panes.length : "no"} panes were measured, expected 1`);
      continue;
    }
    const at = `single pane at ${width}px`;
    if (!isNum(panes[0]?.paneWidth) || Math.abs(panes[0].paneWidth - width) > 2) bad(`${at}: pane width ${panes[0]?.paneWidth} is not ${width}`);
    headerProblems(panes[0], at, width >= STRICT_SINGLE_FROM, bad);
    floorProblems(panes[0], result.singleNoFloor?.[width]?.[0], at, bad);
  }
  return out;
}

/** Throws with every problem when the measurement fails; returns when it passes. */
export function assertLayout(result) {
  const problems = layoutProblems(result);
  if (problems.length) throw new Error(`layout measurement FAILED (${problems.length}):\n  - ${problems.join("\n  - ")}`);
}
