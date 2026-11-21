import type { AcpToolCall } from "./ipc";

type Part = { entries: any[]; toolCalls: Record<string, AcpToolCall> };

/** Slim a saved transcript once its JSON passes this many characters (the synced cell is capped at 4 MiB). */
export const TRANSCRIPT_BUDGET = 3_500_000;
/** The backend refuses a save past this (`TRANSCRIPT_MAX`, 4 MiB). */
export const TRANSCRIPT_HARD_MAX = 4 * 1024 * 1024;

const NOTE_PREFIX = "Older history was slimmed to fit the saved transcript";

export interface SlimResult {
  json: string;
  /** Tool calls that lost their input/output, and images that were replaced by a note. */
  dropped: { toolOutputs: number; images: number };
  /** Even with every payload dropped the transcript is over the backend limit: the save will be refused. */
  tooBig: boolean;
}

const hasImage = (tc: AcpToolCall) => !!tc.content?.some((c) => c.type === "content" && c.content.type === "image");
const IMAGE_NOTE = "Image not kept: the saved transcript was too large.";
// A call an earlier slimming already stripped (only our note left) has no payload to drop again.
const onlyNote = (tc: AcpToolCall) => tc.content?.length === 1 && tc.content[0].type === "content" && tc.content[0].content.type === "text" && tc.content[0].content.text === IMAGE_NOTE;
const hasPayload = (tc: AcpToolCall) => (!!tc.content?.length && !onlyNote(tc)) || tc.rawInput != null || tc.rawOutput != null;

/** `tc` without its payload; a dropped image inside it leaves a visible note. */
function stripCall(tc: AcpToolCall): AcpToolCall {
  const content: AcpToolCall["content"] = hasImage(tc)
    ? [{ type: "content", content: { type: "text", text: IMAGE_NOTE } }]
    : null;
  return { ...tc, content, rawInput: null, rawOutput: null };
}

const noImage = (): any => ({ kind: "info", text: IMAGE_NOTE });

/**
 * The transcript JSON to save. Under the budget it is exactly what `JSON.stringify` gives.
 * Over it, tool payloads and images are dropped oldest first (earlier agents' history, then this
 * agent's) until it fits, so the recent part of the session stays intact, and a note at the top
 * says how many items went.
 */
export function slimTranscript(entries: any[], toolCalls: Record<string, AcpToolCall>, earlier?: Part, budget = TRANSCRIPT_BUDGET): SlimResult {
  const whole = JSON.stringify({ v: 1, earlier, entries, toolCalls });
  const none = { toolOutputs: 0, images: 0 };
  if (whole.length <= budget) return { json: whole, dropped: none, tooBig: false };

  // Our own note from an earlier slimming is rebuilt below, not stacked.
  const isNote = (e: any) => e.kind === "info" && typeof e.text === "string" && e.text.startsWith(NOTE_PREFIX);
  const mainEntries = entries.filter((e) => !isNote(e));

  const parts: Part[] = [];
  if (earlier) parts.push({ entries: [...earlier.entries], toolCalls: { ...earlier.toolCalls } });
  parts.push({ entries: [...mainEntries], toolCalls: { ...toolCalls } });

  // Reserve room for the note we add.
  let total = whole.length + 300;
  const dropped = { toolOutputs: 0, images: 0 };
  for (const p of parts) {
    for (let i = 0; i < p.entries.length && total > budget; i++) {
      const e = p.entries[i];
      if (e.kind === "image") {
        const next = noImage();
        total -= JSON.stringify(e).length - JSON.stringify(next).length;
        p.entries[i] = next;
        dropped.images++;
      } else if (e.kind === "tool") {
        const tc = p.toolCalls[e.id];
        if (!tc || !hasPayload(tc)) continue;
        const next = stripCall(tc);
        total -= JSON.stringify(tc).length - JSON.stringify(next).length;
        p.toolCalls[e.id] = next;
        dropped.toolOutputs++;
        if (hasImage(tc)) dropped.images++;
      }
    }
  }

  const note = {
    kind: "info",
    text: `${NOTE_PREFIX}: the oldest ${dropped.toolOutputs} tool output${dropped.toolOutputs === 1 ? "" : "s"} and ${dropped.images} image${dropped.images === 1 ? "" : "s"} were not kept.`,
  };
  const main = parts[parts.length - 1];
  const json = JSON.stringify({
    v: 1,
    earlier: earlier && { entries: parts[0].entries, toolCalls: parts[0].toolCalls },
    entries: [note, ...main.entries],
    toolCalls: main.toolCalls,
  });
  return { json, dropped, tooBig: new TextEncoder().encode(json).length > TRANSCRIPT_HARD_MAX };
}
