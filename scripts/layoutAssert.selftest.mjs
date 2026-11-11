// The layout rules fail on a failed or empty measurement, with each planted failure going red
// (pb-20261110-10). Run by `pnpm run selftests`; needs no browser.
import { PANES, WIDTHS, assertLayout, layoutProblems } from "./layout-assert.mjs";

let failed = 0;
const equal = (got, want, msg) => {
  if (got !== want) {
    failed++;
    console.error(`FAIL ${msg}: got ${got}, want ${want}`);
  }
};

const pane = (width, i) => ({
  paneWidth: width,
  pastPaneEdge: [],
  // As measured on the clean tree: the cost is there from a pane of about 300 px.
  usage: { shown: width < 300 ? "In 1M · Out 2k" : "In 1M · Out 2k · $1.00", totalsClipped: false, totalsOverlapRingPx: 0 },
  textarea: { clientHeight: 46, scrollHeight: 46, clipped: false },
  queuedRow: i === 0 ? { rowRightMinusPaneRight: -25, childWidths: ["41:Queued"] } : null,
  workingLine: i === 0 || i === 3 ? ["Writing… | full | 78px"] : null,
});
const good = () => ({ panes: Object.fromEntries(WIDTHS.map((w) => [w, Array.from({ length: PANES }, (_, i) => pane(w, i))])) });
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

if (failed) throw new Error(`${failed} layoutAssert check(s) failed`);
console.log("layoutAssert.selftest: ok");
