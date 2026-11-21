/**
 * Checks for slimming a saved transcript (pb-20261107-14), on synthetic data only.
 * Run: node --experimental-strip-types src/lib/transcriptSlim.selftest.ts
 */
import { slimTranscript, TRANSCRIPT_HARD_MAX } from "./transcriptSlim.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const blob = (n: number) => "x".repeat(n);
const call = (i: number, size: number, image = false): any => ({
  toolCallId: `t${i}`,
  title: `call ${i}`,
  content: [
    image
      ? { type: "content", content: { type: "image", data: blob(size), mimeType: "image/png" } }
      : { type: "content", content: { type: "text", text: blob(size) } },
  ],
  rawInput: { cmd: blob(100) },
  rawOutput: blob(100),
});
const build = (n: number, size: number, imageAt: number[] = []) => {
  const toolCalls: Record<string, any> = {};
  const entries: any[] = [];
  for (let i = 0; i < n; i++) {
    toolCalls[`t${i}`] = call(i, size, imageAt.includes(i));
    entries.push({ kind: "tool", id: `t${i}` });
  }
  return { entries, toolCalls };
};
const parse = (json: string) => JSON.parse(json);
const kept = (c: any) => c.content != null || c.rawInput != null || c.rawOutput != null;

// Under budget: byte-identical to a plain stringify.
{
  const { entries, toolCalls } = build(5, 1000);
  const r = slimTranscript(entries, toolCalls, undefined, 1_000_000);
  assert(r.json === JSON.stringify({ v: 1, earlier: undefined, entries, toolCalls }), "under budget is unchanged");
  assert(r.dropped.toolOutputs === 0 && r.dropped.images === 0 && !r.tooBig, "nothing dropped under budget");
}

// Over budget: oldest first, newest intact, note counts what went.
{
  const { entries, toolCalls } = build(10, 10_000); // ~100 KB of payload
  const r = slimTranscript(entries, toolCalls, undefined, 60_000);
  const out = parse(r.json);
  assert(r.json.length <= 60_000, "fits the budget");
  assert(r.dropped.toolOutputs > 0 && r.dropped.toolOutputs < 10, "drops some, not all");
  assert(!kept(out.toolCalls.t0), "the oldest lost its payload");
  assert(kept(out.toolCalls.t9), "the newest keeps its payload");
  const firstKept = Object.keys(out.toolCalls).findIndex((k) => kept(out.toolCalls[k]));
  assert(firstKept === r.dropped.toolOutputs, "drops form an oldest-first prefix");
  assert(out.entries[0].kind === "info" && out.entries[0].text.includes(`${r.dropped.toolOutputs} tool outputs`), "note counts drops");
  assert(out.entries[0].text.includes("oldest"), "note says they are the oldest");
}

// Image entries and tool-call images: dropped oldest first, with a visible note.
{
  const { entries, toolCalls } = build(4, 10_000, [0]);
  entries.unshift({ kind: "image", data: blob(50_000), mimeType: "image/png" });
  const r = slimTranscript(entries, toolCalls, undefined, 30_000);
  const out = parse(r.json);
  const img = out.entries[1];
  assert(img.kind === "info" && img.text.startsWith("Image not kept"), "image entry replaced by a note");
  const t0 = out.toolCalls.t0.content;
  assert(t0 && t0[0].content.type === "text" && t0[0].content.text.startsWith("Image not kept"), "tool-call image leaves a note");
  assert(r.dropped.images === 2, "both images counted");
}

// Earlier agents' history goes before this agent's.
{
  const e = build(5, 10_000);
  const m = build(5, 10_000);
  const r = slimTranscript(m.entries, m.toolCalls, e, 70_000);
  const out = parse(r.json);
  assert(!kept(out.earlier.toolCalls.t0), "earlier history is dropped first");
  assert(kept(out.toolCalls.t4), "the current agent's newest is intact");
}

// Slimming twice does not stack notes.
{
  const { entries, toolCalls } = build(10, 10_000);
  const once = parse(slimTranscript(entries, toolCalls, undefined, 60_000).json);
  const twice = parse(slimTranscript(once.entries, once.toolCalls, undefined, 40_000).json);
  assert(twice.entries.filter((e: any) => e.kind === "info").length === 1, "one note");
}

// Still too big after dropping everything: reported.
{
  const entries = [{ kind: "agent", text: blob(TRANSCRIPT_HARD_MAX + 10) }];
  const r = slimTranscript(entries, {}, undefined, 1000);
  assert(r.tooBig, "unfixable size is reported");
}

console.log("transcriptSlim selftest ok");
