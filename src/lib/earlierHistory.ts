import type { AcpToolCall } from "./ipc";

type Part = { entries: unknown[]; toolCalls: Record<string, AcpToolCall> };

/**
 * The earlier history a window should show once the agent has started a new session: what it
 * already has, else the saved copy. The host may have written that copy after the window loaded
 * the transcript (an import whose real session wouldn't load), and a window that never picks it
 * up saves its own, shorter transcript over it.
 */
export function adoptEarlier<T extends Part>(shown: T | null, saved: T | null): T | null {
  return shown && shown.entries.length > 0 ? shown : saved && saved.entries.length > 0 ? saved : shown;
}
