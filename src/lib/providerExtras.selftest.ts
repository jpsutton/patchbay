/**
 * Checks for the provider extras editor rules.
 * Run: node --experimental-strip-types src/lib/providerExtras.selftest.ts
 */
import { canSaveExtras, extraEntries, type ExtraRow } from "./providerExtras.ts";

function assert(ok: boolean, what: string) {
  if (!ok) throw new Error(`providerExtras selftest failed: ${what}`);
}

assert(!canSaveExtras("loading", false), "no save while the stored names are loading");
assert(!canSaveExtras("failed", false), "no save after a failed load: it would wipe the stored values");
assert(canSaveExtras("loaded", false), "save once loaded");
assert(!canSaveExtras("loaded", true), "no second save while one is running");

const rows: ExtraRow[] = [
  { name: "api-key", value: "", stored: true },
  { name: "  ", value: "orphan", stored: false },
  { name: "", value: "", stored: false },
  { name: " x-team ", value: "t", stored: false },
];
const out = extraEntries(rows);
assert(out.length === 2, "blank-name rows are skipped, none sent as name ''");
assert(out[0].name === "api-key" && out[0].value === undefined, "a blank value keeps the stored one");
assert(out[1].name === "x-team" && out[1].value === "t", "names are trimmed, a typed value is sent");
assert(extraEntries([]).length === 0, "an empty list stays empty (clears on purpose)");
console.log("providerExtras selftest ok");
