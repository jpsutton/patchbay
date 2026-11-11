import { trapTarget } from "./focusTrap.ts";

let failed = 0;
const equal = (got: unknown, want: unknown, msg: string) => {
  if (got !== want) {
    failed++;
    console.error(`FAIL ${msg}: got ${got}, want ${want}`);
  }
};

// Inside the ring the browser's own Tab step is left alone.
equal(trapTarget(3, 0, false), null, "trapTarget(3, 0, false)");
equal(trapTarget(3, 1, true), null, "trapTarget(3, 1, true)");
// Tab on the last control wraps to the first; Shift+Tab on the first wraps to the last.
equal(trapTarget(3, 2, false), 0, "trapTarget(3, 2, false)");
equal(trapTarget(3, 0, true), 2, "trapTarget(3, 0, true)");
// Focus outside the modal is pulled in at the end Tab would have entered from.
equal(trapTarget(3, -1, false), 0, "trapTarget(3, -1, false)");
equal(trapTarget(3, -1, true), 2, "trapTarget(3, -1, true)");
// One control: both directions stay on it.
equal(trapTarget(1, 0, false), 0, "trapTarget(1, 0, false)");
equal(trapTarget(1, 0, true), 0, "trapTarget(1, 0, true)");
// Nothing focusable: the caller keeps focus on the dialog itself.
equal(trapTarget(0, -1, false), null, "trapTarget(0, -1, false)");
if (failed) throw new Error(`${failed} focusTrap check(s) failed`);
console.log("focusTrap.selftest: ok");
