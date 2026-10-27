/**
 * Pure-function checks for the plain-English tool call label.
 * Run: node --experimental-strip-types src/code/toolLabel.selftest.ts
 */
import { toolDescription } from "./toolLabel.ts";

function eq(actual: unknown, expected: unknown, msg: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}

eq(toolDescription({ rawInput: { command: "ls", description: "List files" } }), "List files", "description");
eq(toolDescription({ rawInput: { description: "  Watch CI\nmore detail" } }), "Watch CI", "first line, trimmed");
eq(toolDescription({ rawInput: { description: "   " } }), null, "blank");
eq(toolDescription({ rawInput: { description: 5 } }), null, "not a string");
eq(toolDescription({ rawInput: { command: "ls" } }), null, "no description");
eq(toolDescription({ rawInput: "ls" }), null, "string input");
eq(toolDescription({ rawInput: null }), null, "null input");
eq(toolDescription({}), null, "no input");
console.log("toolLabel selftest ok");
