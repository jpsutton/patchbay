import type { CodeState } from "./store";

/**
 * Steered messages the agent has probably read: it takes one in before its next model request, so
 * once a tool call after the message has finished and the agent goes on, it is no longer waiting.
 * A message that came in while tool calls ran is read as soon as those finish and the agent goes
 * on: those calls sit above it in the transcript, so the rule above would only fire a whole step
 * later, long after the model has seen it. ACP says nothing when a steer is read, so this is a
 * guess from the transcript.
 */
export function readSteers(s: CodeState): CodeState {
  if (!s.entries.some((x) => x.kind === "user" && x.pending)) return s;
  const done = (id: string) => {
    const status = s.toolCalls[id]?.status;
    return status === undefined || status === "completed" || status === "failed";
  };
  let stepped = false;
  let entries = s.entries;
  for (let i = entries.length - 1; i >= 0; i--) {
    const x = entries[i];
    if (x.kind === "tool") {
      const status = s.toolCalls[x.id]?.status;
      if (status === "completed" || status === "failed") stepped = true;
    } else if (x.kind === "user" && x.pending && (stepped || (x.waitFor?.length && x.waitFor.every(done)))) {
      if (entries === s.entries) entries = [...entries];
      entries[i] = { ...x, pending: false, waitFor: undefined };
    }
  }
  return entries === s.entries ? s : { ...s, entries };
}
