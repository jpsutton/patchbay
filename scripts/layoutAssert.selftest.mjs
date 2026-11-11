// The layout rules fail on a failed or empty measurement, with each planted failure going red
// (pb-20261110-10). Run by `pnpm run selftests`; needs no browser.
import { PANES, SINGLE_WIDTHS, WIDTHS, assertLayout, layoutProblems } from "./layout-assert.mjs";

let failed = 0;
const equal = (got, want, msg) => {
  if (got !== want) {
    failed++;
    console.error(`FAIL ${msg}: got ${got}, want ${want}`);
  }
};

const ctl = (kind, width) => ({ kind, where: "subheader", width, pastPaneEdgePx: -40, minWidth: "auto", truncated: false });
const header = () => ({ mode: ctl("chip", 70), approval: ctl("radiogroup", 232), headerOverflow: false, subheaderOverflow: false, collisions: [] });
const pane = (width, i) => ({
  paneWidth: width,
  headerControls: header(),
  pastPaneEdge: [],
  // As measured on the clean tree: the cost is there from a pane of about 300 px.
  usage: { shown: width < 300 ? "In 1M · Out 2k" : "In 1M · Out 2k · $1.00", totalsClipped: false, totalsOverlapRingPx: 0 },
  textarea: { clientHeight: 46, scrollHeight: 46, clipped: false },
  queuedRow: i === 0 ? { rowRightMinusPaneRight: -25, childWidths: ["41:Queued"] } : null,
  workingLine: i === 0 || i === 3 ? ["Writing… | full | 78px"] : null,
});
const set = (widths, n) => Object.fromEntries(widths.map((w) => [w, Array.from({ length: n }, (_, i) => pane(w, i))]));
const good = () => ({ panes: set(WIDTHS, PANES), noFloor: set(WIDTHS, PANES), single: set(SINGLE_WIDTHS, 1), singleNoFloor: set(SINGLE_WIDTHS, 1) });
const throws = (r) => {
  try {
    assertLayout(r);
    return false;
  } catch {
    return true;
  }
};
const plant = (name, mutate) => {
  const r = good();
  mutate(r);
  equal(throws(r), true, `planted failure "${name}" must go red`);
  equal(layoutProblems(r).length > 0, true, `planted failure "${name}" must name a problem`);
};

// The clean measurement passes, so the planted ones below are red for their own reason.
equal(throws(good()), false, "a clean measurement passes");

// A failed or empty measurement is never a pass (the original gap).
equal(throws(undefined), true, "no result");
equal(throws({}), true, "no panes key");
equal(throws({ panes: {} }), true, "no widths measured");
equal(throws({ panes: Object.fromEntries(WIDTHS.map((w) => [w, []])) }), true, "every width measured zero panes");
plant("three panes at 250", (r) => r.panes[250].pop());
plant("a width missing", (r) => delete r.panes[340]);
plant("pane record null", (r) => (r.panes[180][2] = null));
plant("wrong pane width", (r) => (r.panes[500][1].paneWidth = 340));
plant("past-the-edge probe returned nothing", (r) => (r.panes[250][0].pastPaneEdge = undefined));
plant("usage probe found nothing", (r) => (r.panes[340][3].usage = null));
plant("ring probe found nothing", (r) => (r.panes[340][3].usage.totalsOverlapRingPx = null));
plant("textarea probe found nothing", (r) => (r.panes[180][1].textarea = null));
plant("queued row never injected", (r) => (r.panes[250][0].queuedRow = null));
plant("working line never injected", (r) => (r.panes[500][3].workingLine = null));
// The two regressions #412 fixed, as planted in its source (pb-20261110-10).
plant("the cost shows in a 180px pane (UsageBar's cost spans made plain)", (r) => (r.panes[180][0].usage.shown = "In 1M · Out 2k · $162.07"));
plant("the cost shows in a 250px pane", (r) => (r.panes[250][2].usage.shown = "In 1M · Out 2k · $162.07"));
plant("the cost is gone from a 500px pane", (r) => (r.panes[500][1].usage.shown = "In 1M · Out 2k"));
plant("the working-line label truncates (WorkingIndicator's min width removed)", (r) => (r.panes[180][0].workingLine = ["Writing… | TRUNCATED | 29px", "6h 59m · ~2.7k tokens | TRUNCATED | 31px"]));
plant("the working-line label truncates on pane 4", (r) => (r.panes[250][3].workingLine = ["Thinking… | TRUNCATED | 40px", "48s | full | 102px"]));
{
  // The timer after the label truncates in the clean run at 180 and 250 px: not a failure.
  const r = good();
  r.panes[180][0].workingLine = ["Writing… | full | 78px", "6h 59m · ~2.7k tokens | TRUNCATED | 31px"];
  r.panes[250][3].workingLine = ["Thinking… | full | 78px", "48s · ~550 tokens | TRUNCATED | 102px"];
  equal(throws(r), false, "a truncated timer is not a failure");
}
// A real layout failure is red too.
plant("an element past the pane edge", (r) => (r.panes[180][1].pastPaneEdge = ["span +414.6px"]));
plant("usage over the ring", (r) => (r.panes[250][2].usage.totalsOverlapRingPx = 12));
plant("clipped composer text", (r) => (r.panes[180][0].textarea = { clientHeight: 70, scrollHeight: 94, clipped: true }));
plant("queued row past the pane", (r) => (r.panes[250][0].queuedRow.rowRightMinusPaneRight = 31));

// The header's mode and approval controls (pb-20261110-09).
plant("header probe found no header", (r) => (r.panes[250][1].headerControls = null));
plant("mode control not found", (r) => (r.panes[340][0].headerControls.mode = null));
plant("approval control not found", (r) => (r.panes[500][2].headerControls.approval = null));
plant("the mode chip squeezed to 2px (its floor removed)", (r) => (r.single[600][0].headerControls.mode.width = 2));
plant("the approval control squeezed to 30px", (r) => (r.panes[340][1].headerControls.approval.width = 30));
plant("collision probe returned nothing", (r) => (r.panes[250][3].headerControls.collisions = undefined));
plant("controls overlap each other", (r) => (r.panes[180][0].headerControls.collisions = ["0x1"]));
plant("a control past the pane edge at 250", (r) => (r.panes[250][0].headerControls.approval.pastPaneEdgePx = 12));
plant("the header overflows at 340", (r) => (r.panes[340][2].headerControls.headerOverflow = true));
plant("the sub-header overflows at 500", (r) => (r.panes[500][1].headerControls.subheaderOverflow = true));
plant("no-floor measurement missing", (r) => delete r.noFloor[250]);
plant("no-floor pane record missing", (r) => (r.noFloor[340][1] = null));
plant("single pane never measured", (r) => delete r.single[720]);
plant("two panes where one single pane was expected", (r) => r.single[600].push(pane(600, 1)));
plant("single pane header overflows at 720", (r) => (r.single[720][0].headerControls.headerOverflow = true));
plant("single pane header found nothing", (r) => (r.single[900][0].headerControls = null));
// The chip's floor must not make a pane worse than it is without the floor.
// (at 180px and 600px single, where an overflow alone is not asserted, so each is red for the floor's sake)
plant("the floor makes the sub-header overflow", (r) => {
  r.panes[180][0].headerControls.subheaderOverflow = true;
});
plant("the floor makes the single-pane header overflow at 600", (r) => {
  r.single[600][0].headerControls.headerOverflow = true;
});
plant("the floor pushes a control further past the edge at 180", (r) => {
  r.panes[180][1].headerControls.mode.pastPaneEdgePx = 30;
  r.noFloor[180][1].headerControls.mode.pastPaneEdgePx = 5;
});
plant("controls overlap at 180 (always a failure)", (r) => (r.panes[180][2].headerControls.collisions = ["0x1"]));
for (const [name, mutate] of [
  ["sub-header overflow at 180", (r) => (r.panes[180][0].headerControls.subheaderOverflow = true)],
  ["header overflow at single 600", (r) => (r.single[600][0].headerControls.headerOverflow = true)],
  ["past-edge at 180", (r) => (r.panes[180][1].headerControls.mode.pastPaneEdgePx = 30)],
]) {
  // Without the floor the same state is also there: nothing for the floor to answer for.
  const r = good();
  mutate(r);
  const sameOff = good();
  mutate(sameOff);
  r.noFloor = sameOff.panes;
  r.singleNoFloor = sameOff.single;
  equal(layoutProblems(r).some((p) => p.includes("floor")), false, `${name}: not blamed on the floor when it is there without it`);
}
// What is measured and known (stated in the PR): a pre-existing overflow that the floor does not cause is not a floor failure.
{
  const r = good();
  r.panes[180][0].headerControls.approval.pastPaneEdgePx = 64.5;
  r.noFloor[180][0].headerControls.approval.pastPaneEdgePx = 64.5;
  r.panes[180][0].headerControls.subheaderOverflow = true;
  r.noFloor[180][0].headerControls.subheaderOverflow = true;
  r.single[600][0].headerControls.headerOverflow = true;
  r.singleNoFloor[600][0].headerControls.headerOverflow = true;
  equal(throws(r), false, "the narrowest panes' overflow, with and without the floor, is reported, not failed");
}

if (failed) throw new Error(`${failed} layoutAssert check(s) failed`);
console.log("layoutAssert.selftest: ok");
