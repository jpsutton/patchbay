/**
 * Run: node --experimental-strip-types src/lib/quitWaitPoll.selftest.ts
 *
 * pb-20261111-66: quit_wait_status is polled only by the native desktop shell and only
 * while the wait dialog is showing. The web UI must make ZERO calls (the daemon would answer
 * "unknown command quit_wait_status" 4x/s per polling window), and a closed dialog must not
 * poll forever. The wiring assertions pin the shipped component to this gate — the bug was
 * wiring, not arithmetic.
 */
// @ts-expect-error — node:fs has no types in this app config (types: [vite/client]); this
// selftest runs under `node --experimental-strip-types`, not in the app bundle.
import { readFileSync } from "node:fs";
import { shouldPollQuitWait } from "./quitWaitPoll.ts";

function assert(cond: boolean, what: string) {
  if (!cond) throw new Error(`quitWaitPoll selftest failed: ${what}`);
}

// (1) The web UI makes ZERO quit_wait_status calls: never allowed to poll, showing or not,
// simulated as a long tick loop (a 500ms poll over the dialog's whole life).
let webCalls = 0;
for (let i = 0; i < 1_000; i++) {
  if (shouldPollQuitWait({ desktop: false, showing: true })) webCalls++;
}
assert(webCalls === 0, `web UI must make zero quit_wait_status calls, got ${webCalls}`);
assert(
  !shouldPollQuitWait({ desktop: false, showing: false }),
  "web never polls when hidden either",
);

// (2) The desktop path polls while the dialog shows…
let shownTicks = 0;
for (let i = 0; i < 1_000; i++) {
  if (shouldPollQuitWait({ desktop: true, showing: true })) shownTicks++;
}
assert(shownTicks === 1_000, `desktop polls every tick while showing, got ${shownTicks}/1000`);

// …and stops the moment it is not showing (Keep Patchbay open, cancelled or finished quit).
let hiddenTicks = 0;
for (let i = 0; i < 1_000; i++) {
  if (shouldPollQuitWait({ desktop: true, showing: false })) hiddenTicks++;
}
assert(hiddenTicks === 0, `the poll stops when the dialog is not showing, got ${hiddenTicks}`);

// Wiring: the shipped dialog must go through this gate (the interval effect depends on
// [desktop, waiting]) and its mount one-shot must be desktop-gated — the original bug was an
// ungated setInterval in a component mounted in every window, including patchbayd's web UI.
const src = readFileSync(new URL("../components/QuitWaitDialog.tsx", import.meta.url), "utf8");
assert(src.includes("shouldPollQuitWait("), "QuitWaitDialog polls through the gate");
assert(src.includes("}, [desktop, waiting]);"), "the interval effect is gated on showing");
assert(src.includes("if (!desktop) return;"), "the mount one-shot is desktop-gated");
assert(
  !/setInterval\(sync, 500\);\s*\n\s*const un = listen/.test(src),
  "no ungated forever-interval paired with the event listener",
);

console.log("quitWaitPoll selftest: ok");
