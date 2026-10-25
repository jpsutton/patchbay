/**
 * Run: node --experimental-strip-types src/lib/spokenText.selftest.ts
 */
import { liveCursorNow, proseSinceLastUser, speechChunks, speechSegments, spokenText, takeLiveProse } from "./spokenText.ts";

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

const long = Array.from({ length: 40 }, (_, i) => `This is sentence number ${i} of a long reply.`).join(" ");
const ramp = speechChunks(long, 1500);
assert(ramp[0].length <= 160, `first piece is short, got ${ramp[0].length}`);
assert(ramp.length > 2 && ramp[1].length > ramp[0].length, "later pieces grow");
assert(ramp.every((c) => c.length <= 1500), "no piece passes max");
assert(ramp.join(" ") === long, "no words lost or reordered");
assert(speechChunks("One. Two.", 320, 4)[0] === "One.", "first piece honors its own size");

const md = "Intro paragraph.\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\nOutro paragraph.";
const whole = speechSegments(md, 10_000);
assert(whole.length === 1 && whole[0] === md, "short reply is one segment, unchanged");
const split = speechSegments(md, 5);
assert(split.length === 3, `a fence with a blank line stays whole, got ${split.length}`);
assert(split[1].startsWith("```ts") && split[1].endsWith("```"), "fence kept in one segment");
const para = Array.from({ length: 30 }, (_, i) => `Paragraph ${i} has a few words in it.`).join("\n\n");
const segs = speechSegments(para, 100, 800);
assert(segs[0].length <= 140 && segs.length > 2 && segs[1].length > segs[0].length, "segments grow after a short first");
assert(segs.every((x) => x.length <= 800 + 60), "no segment far past max");
assert(segs.join("\n\n") === para, "no text lost or reordered");

const prose = proseSinceLastUser([
  { kind: "user", text: "first" },
  { kind: "agent", text: "old" },
  { kind: "user", text: "second" },
  { kind: "thought", text: "hidden" },
  { kind: "agent", text: "new" },
  { kind: "tool" },
]);
assert(prose === "new", `only the latest turn's prose, got ${prose}`);

// Live reading: a message is spoken once something follows it, the last one once settled.
const turn: { kind: string; text?: string }[] = [{ kind: "user", text: "go" }];
let cur = liveCursorNow(turn);
assert(cur.next === 1, "starts after the prompt");
turn.push({ kind: "agent", text: "First note." });
let got = takeLiveProse(turn, cur, false);
assert(got.texts.length === 0, "the message still being written waits");
turn.push({ kind: "tool" }, { kind: "agent", text: "Second" });
got = takeLiveProse(turn, cur, false);
assert(got.texts.join("|") === "First note.", `first message done once a tool follows, got ${got.texts}`);
cur = got.cursor;
turn[3] = { kind: "agent", text: "Second note." };
got = takeLiveProse(turn, cur, false);
assert(got.texts.length === 0, "nothing new while the last is unsettled");
got = takeLiveProse(turn, cur, true);
assert(got.texts.join("|") === "Second note.", `settled turn speaks the last message, got ${got.texts}`);
cur = got.cursor;
turn[3] = { kind: "agent", text: "Second note. And a late tail." };
got = takeLiveProse(turn, cur, true);
assert(got.texts.join("|").trim() === "And a late tail.", `late text speaks only the new part, got ${got.texts}`);
assert(takeLiveProse(turn, got.cursor, true).texts.length === 0, "nothing read twice");
assert(takeLiveProse([{ kind: "user" }], { next: 9, spoken: 3 }, true).texts.length === 0, "a cut-back list is safe");
assert(liveCursorNow([{ kind: "user" }, { kind: "agent", text: "mid" }]).next === 1, "joining mid-message reads that message");

console.log("spokenText.selftest: ok");
