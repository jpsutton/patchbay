/**
 * Pure-function checks for sorting replayed user messages.
 * Run: node --experimental-strip-types src/lib/harness.selftest.ts
 */
import type { Entry } from "./store";
import { appendUserChunk, harnessTags } from "./harness.ts";

function eq(actual: unknown, expected: unknown, msg: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}\n  got:  ${a}\n  want: ${e}`);
}

const replay = (chunks: string[], start: Entry[] = []) => chunks.reduce(appendUserChunk, start);
const reminder = "<system-reminder>\nThe user opened a.ts.\n</system-reminder>";

// Whole-message cases, one chunk each.
eq(replay([reminder]), [{ kind: "harness", text: reminder }], "wrapper-only chunk is harness");
eq(replay(["[Request interrupted by user for tool use]"]), [{ kind: "cancelled" }], "cancel marker");
eq(replay(["Use <b>bold</b> here"]), [{ kind: "user", text: "Use <b>bold</b> here" }], "plain HTML stays a prompt");
eq(
  replay(["Use <my-tag>x</my-tag> here"]),
  [{ kind: "user", text: "Use <my-tag>x</my-tag> here" }],
  "inline hyphenated element stays in the prompt",
);

// A wrapper split across chunks of one message.
eq(
  replay(["<system-reminder>\nThe user ", "opened a.ts.\n</system-", "reminder>"]),
  [{ kind: "harness", text: reminder }],
  "split wrapper becomes one harness entry",
);
eq(
  replay(["Fix the test.", "\n<system-reminder>\nThe user ", "opened a.ts.\n</system-reminder>"]),
  [{ kind: "user", text: "Fix the test." }, { kind: "harness", text: reminder }],
  "split trailing wrapper leaves the prompt",
);

// A chunk mixing a prompt and a wrapper.
eq(
  replay([`Fix X\n${reminder}`]),
  [{ kind: "user", text: "Fix X" }, { kind: "harness", text: reminder }],
  "trailing wrapper peeled off",
);
eq(
  replay([`${reminder}\nFix X`]),
  [{ kind: "harness", text: reminder }, { kind: "user", text: "Fix X" }],
  "leading wrapper peeled off",
);
eq(
  replay([`${reminder}\nFix X\n${reminder}`]),
  [{ kind: "harness", text: reminder }, { kind: "user", text: "Fix X" }, { kind: "harness", text: reminder }],
  "both ends peeled off",
);
eq(
  replay([`Fix X\n${reminder}\nand Y`]),
  [{ kind: "user", text: `Fix X\n${reminder}\nand Y` }],
  "a wrapper inside the prompt stays",
);

// Separate messages that arrive back to back, with no line break between them.
eq(
  replay(["Fix the flaky upload test.", reminder]),
  [{ kind: "user", text: "Fix the flaky upload test." }, { kind: "harness", text: reminder }],
  "wrapper in its own chunk after a prompt",
);
eq(
  replay(["[Request interrupted by user]", "Use <b>bold</b> instead."]),
  [{ kind: "cancelled" }, { kind: "user", text: "Use <b>bold</b> instead." }],
  "cancel then prompt",
);
eq(
  replay(["<command-name>/model</command-name>\n<command-args>opus</command-args>", "<local-command-stdout>Set</local-command-stdout>"]),
  [
    { kind: "harness", text: "<command-name>/model</command-name>\n<command-args>opus</command-args>" },
    { kind: "harness", text: "<local-command-stdout>Set</local-command-stdout>" },
  ],
  "each chunk's wrappers are their own entry",
);

// A prompt streamed in pieces stays one entry, whitespace intact.
eq(replay(["Fix the ", "flaky ", "test."]), [{ kind: "user", text: "Fix the flaky test." }], "streamed prompt");

// After other output the next user chunk starts a new message.
const afterAgent = replay(["Hi"], [...replay([reminder]), { kind: "agent", text: "ok" }]);
eq(afterAgent, [{ kind: "harness", text: reminder }, { kind: "agent", text: "ok" }, { kind: "user", text: "Hi" }], "new run after agent text");

// A user entry from elsewhere (a live echo) takes the chunk and keeps its fields.
eq(
  replay([" more"], [{ kind: "user", text: "Fix", promptId: "p1" }]),
  [{ kind: "user", text: "Fix more", promptId: "p1" }],
  "adopted user entry keeps its id",
);

eq(harnessTags(reminder), ["system-reminder"], "harnessTags names the element");
eq(harnessTags("Fix X"), null, "harnessTags rejects prose");

console.log("harness selftest: ok");
