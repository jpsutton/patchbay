/**
 * Run: node --experimental-strip-types src/lib/spokenText.selftest.ts
 */
import { proseSinceLastUser, speechChunks, spokenText } from "./spokenText.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const spoken = spokenText(
  "Hello [docs](https://example.com).\n\n```ts\nconst n = 1;\n```\n\n```\n" +
    "a\nb\nc\nd\ne\nf\ng\nh\ni\n```\n\n```mermaid\ngraph TD\nA-->B\n```\n\n| a | b |\n|---|---|\n\n**done**",
);
assert(spoken.includes("Hello docs."), `link label kept: ${spoken}`);
assert(!spoken.includes("https://"), "url dropped");
assert(spoken.includes("Code: const n = 1;"), `short fence spoken: ${spoken}`);
assert(spoken.includes("Code block."), `long fence skipped: ${spoken}`);
assert(spoken.includes("Diagram."), `mermaid skipped: ${spoken}`);
assert(!spoken.includes("|"), "table dropped");
assert(spoken.includes("done"), "emphasis stripped to the word");

const chunks = speechChunks("One. Two is longer! Three?");
assert(chunks.length === 1 && chunks[0].includes("Three?"), `short sentences stay one piece, got ${chunks.join(" | ")}`);
assert(speechChunks("One. Two.", 4)[0] === "One.", "a sentence ends a full piece");
assert(speechChunks("x".repeat(700), 320).every((c) => c.length <= 320), "long run is split");

const prose = proseSinceLastUser([
  { kind: "user", text: "first" },
  { kind: "agent", text: "old" },
  { kind: "user", text: "second" },
  { kind: "thought", text: "hidden" },
  { kind: "agent", text: "new" },
  { kind: "tool" },
]);
assert(prose === "new", `only the latest turn's prose, got ${prose}`);

console.log("spokenText.selftest: ok");
