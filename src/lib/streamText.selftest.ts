/**
 * How streamed reply text and thinking pieces become transcript entries (pb-20261107-20).
 * Run: node --experimental-strip-types src/lib/streamText.selftest.ts
 */
import type { Entry } from "./store";
import { appendText } from "./streamText.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}
function same(actual: unknown, expected: unknown, msg: string) {
  assert(
    JSON.stringify(actual) === JSON.stringify(expected),
    `${msg}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`,
  );
}

type Piece = ["agent" | "thought", string];
const stream = (start: Entry[], pieces: Piece[]) =>
  pieces.reduce((entries, [kind, text]) => appendText(entries, kind, text), start);
const prompt: Entry = { kind: "user", text: "hi" };

// The report: a model that alternates small text and thinking pieces. One reply, one bubble.
same(
  stream(
    [prompt],
    [
      ["thought", "Plan the answer."],
      ["agent", "Here is"],
      ["thought", " checking"],
      ["agent", " the first part"],
      ["thought", " more"],
      ["agent", " and"],
      ["thought", "\n"],
      ["agent", " the rest."],
    ],
  ),
  [prompt, { kind: "thought", text: "Plan the answer. checking more\n" }, { kind: "agent", text: "Here is the first part and the rest." }],
  "alternating pieces make one Thinking block and one text bubble",
);

// Text first, thinking only mid-reply: the thinking goes in just before the text.
same(
  stream([prompt], [["agent", "One"], ["thought", "hmm"], ["agent", " two"]]),
  [prompt, { kind: "thought", text: "hmm" }, { kind: "agent", text: "One two" }],
  "a thought mid-reply with no thinking before it is put before the text",
);

// A whitespace-only thinking piece starts no entry, at the start or mid-reply.
same(stream([prompt], [["thought", "  \n"], ["agent", "Hi"]]), [prompt, { kind: "agent", text: "Hi" }], "leading blank thought dropped");
same(
  stream([prompt], [["agent", "Hi"], ["thought", "\n"], ["agent", " there"]]),
  [prompt, { kind: "agent", text: "Hi there" }],
  "blank thought mid-reply dropped",
);

// Anything else between them ends the run: text after a tool call is a new bubble, and a
// thought after that text is not folded into the thinking before the tool call.
const tool: Entry = { kind: "tool", id: "t1" };
same(
  stream([prompt, { kind: "thought", text: "a" }, { kind: "agent", text: "b" }, tool], [["agent", "c"], ["thought", "d"], ["agent", "e"]]),
  [prompt, { kind: "thought", text: "a" }, { kind: "agent", text: "b" }, tool, { kind: "thought", text: "d" }, { kind: "agent", text: "ce" }],
  "a tool call ends the reply's run",
);

// Unchanged: same-kind pieces join, empty pieces do nothing, a reply's first text keeps its time.
same(appendText([prompt], "agent", ""), [prompt], "an empty piece changes nothing");
same(appendText([prompt], "agent", "x", 5), [prompt, { kind: "agent", text: "x", at: 5 }], "a new bubble takes its time");
same(
  appendText([prompt, { kind: "agent", text: "x", at: 5 }], "thought", "t"),
  [prompt, { kind: "thought", text: "t" }, { kind: "agent", text: "x", at: 5 }],
  "the moved-past bubble keeps its time",
);
same(stream([], [["thought", "a"], ["thought", "b"]]), [{ kind: "thought", text: "ab" }], "thinking pieces join");

console.log("streamText selftest ok");
