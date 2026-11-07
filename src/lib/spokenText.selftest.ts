/**
 * Run: node --experimental-strip-types src/lib/spokenText.selftest.ts
 */
import {
  liveCursorNow,
  proseSinceLastUser,
  questionFormSpeech,
  questionSpeech,
  speechChunks,
  speechSegments,
  spokenText,
  takeLiveProse,
  takeLiveQuestions,
} from "./spokenText.ts";

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

// Question forms: the serializer reads the prompt, every option, and the card's hints.
const q1 = {
  prompt: "**Where** should we deploy [first](https://example.com)?",
  options: [
    { label: "Staging", description: "Safer, slower" },
    { label: "Production", description: null },
  ],
  multi: true,
  freeText: true,
  skippable: true,
};
const q1text = questionSpeech(q1);
const q1spoken = spokenText(q1text);
assert(q1spoken.includes("Where should we deploy first?"), `prompt cleaned by spokenText: ${q1spoken}`);
assert(q1spoken.includes("Choose any that apply."), "multi-select hint");
assert(q1spoken.includes("Option 1: Staging. Safer, slower"), `option + description: ${q1spoken}`);
assert(q1spoken.includes("Option 2: Production"), "second option");
assert(q1spoken.includes("Something else"), "free-text hint");
assert(q1spoken.includes("You can skip this question."), "skip hint");
assert(!q1spoken.includes("https://") && !q1spoken.includes("**"), "markdown stripped");

const q2 = { prompt: "Pick one.", options: [{ label: "Yes" }, { label: "No" }] };
const q2text = questionSpeech(q2);
assert(!q2text.includes("Choose any"), "no multi hint when single-select");
assert(!q2text.includes("Something else"), "no free-text hint when absent");
const formText = questionFormSpeech({ questions: [q1, q2] });
assert(formText.startsWith(q1text) && formText.includes("Pick one."), "whole form keeps both questions in order");

// Live question reading: pending-at-join is history, a new arrival is spoken once.
type QForm = { requestId: string; form: { questions: typeof q1[] } };
const form = (requestId: string): QForm => ({ requestId, form: { questions: [q1] } });
const pending = [form("r1")];
let qc = takeLiveQuestions(pending, null, "s1", true);
assert(qc.forms.length === 0, "a question pending when the view opens is not spoken");
qc = takeLiveQuestions(pending, qc.cursor, "s1", true);
assert(qc.forms.length === 0, "no double-speak on re-render");
const arrived = [...pending, form("r2")];
qc = takeLiveQuestions(arrived, qc.cursor, "s1", true);
assert(qc.forms.length === 1 && qc.forms[0].requestId === "r2", `one new arrival is spoken once: ${JSON.stringify(qc.forms.map((f) => f.requestId))}`);
assert(qc.forms[0].text === questionFormSpeech(form("r2").form), "the arrival carries the form's speech text");
qc = takeLiveQuestions(arrived, qc.cursor, "s1", true);
assert(qc.forms.length === 0, "an arrival is never spoken twice");
// Answering / skipping removes the form; that must not trigger speech.
qc = takeLiveQuestions([], qc.cursor, "s1", true);
assert(qc.forms.length === 0, "answering/skipping a question triggers no speech");
// Not listening, a session change, and a rejoin all re-baseline to history.
assert(takeLiveQuestions(arrived, qc.cursor, "s1", false).cursor === null, "not listening clears the cursor");
const rejoined = takeLiveQuestions(arrived, qc.cursor, "s2", true);
assert(rejoined.forms.length === 0, "rejoining under a new session reads nothing from history");
const idleThenActive = takeLiveQuestions(arrived, null, "s1", true);
assert(idleThenActive.forms.length === 0, "a fresh cursor baselines everything pending");

console.log("spokenText.selftest: ok");
