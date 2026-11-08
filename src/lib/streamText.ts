import type { Entry } from "./store";

/**
 * `entries` with a streamed piece of the agent's reply text or thinking added (pb-20261107-20).
 *
 * A piece joins the last entry when it is of the same kind. A thinking piece that arrives while
 * the reply text is streaming goes into the thinking just before that text, or a new thinking
 * entry put there, so the text stays last and its next piece joins the same bubble: a model that
 * switches between short text and thinking pieces (mimo does) still gets one Thinking block and
 * one text bubble, as its saved message has. Anything else between them (a tool call, an image,
 * a prompt) ends the reply's run as before. A whitespace-only thinking piece starts no entry.
 */
export function appendText(entries: Entry[], kind: "agent" | "thought" | "user", text: string, at?: number): Entry[] {
  if (!text) return entries;
  const n = entries.length;
  const last = entries[n - 1];
  if (last && last.kind === kind) {
    return [...entries.slice(0, -1), { ...last, text: last.text + text }];
  }
  if (kind === "thought" && last?.kind === "agent") {
    const before = entries[n - 2];
    if (before?.kind === "thought") {
      return [...entries.slice(0, -2), { ...before, text: before.text + text }, last];
    }
    if (!text.trim()) return entries;
    return [...entries.slice(0, -1), { kind: "thought", text }, last];
  }
  if (kind === "thought" && !text.trim()) return entries;
  if ((kind === "agent" || kind === "user") && at) return [...entries, { kind, text, at }];
  return [...entries, { kind, text } as Entry];
}
