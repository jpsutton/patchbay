/**
 * The window's queue-mirror rules (pb-20261107-30), extracted pure so they run under the
 * node selftests without loading the app store.
 *
 * A restore-unqueue (a stop or an error) takes only THIS window's prompts back to the
 * composer; agent-origin entries keep their place for the next turn. A hard drop (user
 * removal, MCP accept, pre-stop drain) removes everything listed — each of those having
 * already told its sender. Entries are id-keyed, so a rename never touches them (the title
 * branch of the store writes only the title and the session list — pinned by source in the
 * selftest).
 */
export interface UnqueueMirror {
  promptIds: string[];
  restore: boolean;
}

/** The new queue after an `unqueued` event (generic: the store's QueuedPrompt and the
 * selftest's minimal fixtures alike). */
export function applyUnqueued<T extends { promptId: string; origin: string }>(
  queued: T[],
  e: UnqueueMirror,
  me: string | undefined,
): T[] {
  if (!e.restore) {
    return queued.filter((q) => !e.promptIds.includes(q.promptId));
  }
  return queued.filter((q) => !(e.promptIds.includes(q.promptId) && q.origin === me));
}
