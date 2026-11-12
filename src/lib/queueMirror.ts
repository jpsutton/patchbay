/**
 * The window's queue-mirror rules (pb-20261107-30), extracted pure so they run under the
 * node selftests without loading the app store.
 *
 * The BACKEND decides which entries leave: a stop/error split emits exactly the non-agent
 * ids it removed from the queue, and hard drops (user removal, MCP accept, pre-stop drain)
 * list what they took. The mirror therefore drops every listed id — keeping any would ghost
 * rows for prompts sent from another computer's UI (origin = that node id, not this
 * window's), which Unqueue on a ghost cannot clear. `handBack`'s `me` filter is the only
 * "mine only" step: only this window's prompts go back to the composer. Entries are
 * id-keyed, so a rename never touches them (the title branch of the store writes only the
 * title and the session list — pinned by source in the selftest).
 */
import type { ComposerRestore, QueuedPrompt } from "./store";

export interface UnqueueMirror {
  promptIds: string[];
  restore: boolean;
}

/** The new queue after an `unqueued` event: every id the backend listed has left it. */
export function applyUnqueued<T extends { promptId: string }>(queued: T[], promptIds: string[]): T[] {
  return queued.filter((q) => !promptIds.includes(q.promptId));
}

/** What a composer gets back when queued prompts it sent won't be: only this window's. */
export function handBack(
  prev: ComposerRestore | null,
  dropped: QueuedPrompt[],
  me: string | undefined,
): ComposerRestore | null {
  const mine = dropped.filter((q) => q.origin === me && !q.replayed);
  if (mine.length === 0) return prev;
  return {
    text: [prev?.text, ...mine.map((q) => q.text)].filter((t) => t?.trim()).join("\n\n"),
    attachments: [...(prev?.attachments ?? []), ...mine.flatMap((q) => q.attachments ?? [])],
  };
}
