/**
 * Run: node --experimental-strip-types src/lib/moveDialogFolder.selftest.ts
 *
 * pb-20261124-10: the move dialog offers the old host's folder only where it is a full path.
 * 1. The pre-fill is blank when the old host's folder doesn't fit the target's OS.
 * 2. Changing the target re-checks an untouched pre-fill, not a folder the user typed.
 * 3. A typed folder in the wrong form keeps Move disabled.
 * The decision itself (fullPathOn) is unit-tested in pathOf.selftest.ts; these anchor its use.
 */
// @ts-expect-error — node:fs has no types in this app config
import { readFileSync } from "node:fs";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(`moveDialogFolder.selftest: ${msg}`);
}

const src = readFileSync(new URL("../code/HostControls.tsx", import.meta.url), "utf8");
const at = src.indexOf("export function MoveDialog(");
assert(at >= 0, "HostControls.tsx defines MoveDialog");
const end = src.indexOf("export function OfflineHostBanner(", at);
assert(end > at, "found the end of MoveDialog");
const dialog = src.slice(at, end);

// (1)
assert(dialog.includes("fullPathOn(session.cwd, osOf(id)) !== false"), "fits() asks fullPathOn about the old host's folder");
assert(dialog.includes('useState(() => (fits(target) ? session.cwd : ""))'), "pre-fill is blank when the folder doesn't fit");
assert(!dialog.includes("useState(session.cwd)"), "no unconditional pre-fill of the old host's folder");
// (2)
assert(dialog.includes("onChange={(e) => pickTarget(e.target.value)}"), "the computer picker goes through pickTarget");
assert(dialog.includes('if (cwd === session.cwd || cwd === "") setCwd(fits(id) ? session.cwd : "")'), "only an untouched pre-fill follows the target");
// (3)
assert(dialog.includes("fullPathOn(cwd, targetOs) === false"), "wrongForm checks the typed folder against the target's OS");
assert(/disabled=\{busy \|\| !cwd\.trim\(\) \|\| wrongForm \|\|/.test(dialog), "Move is disabled while the folder is in the wrong form");

console.log("moveDialogFolder.selftest: ok");
