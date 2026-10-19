/**
 * Whether the Code UI should show Working… — broader than prompt-RPC `busy`.
 *
 * After `session/prompt` returns, some agents (notably Claude Code) still stream
 * autonomous follow-ups when a backgrounded command finishes. Those updates are
 * not tied to another PromptDone, so we treat recent foreground activity and
 * in-progress non-background tools as working too.
 */

import type { AcpToolCall, AcpUpdate } from "./ipc";

/** How long after the last foreground chunk Working… still shows. */
export const FOREGROUND_IDLE_MS = 1500;

type TerminalMeta = {
  terminal_info?: unknown;
  terminal_output?: unknown;
  terminal_output_delta?: unknown;
  terminal_exit?: unknown;
};

/** True when this update is agent work, not only background terminal streaming. */
export function isForegroundUpdate(
  u: AcpUpdate,
  prev?: AcpToolCall,
): boolean {
  switch (u.sessionUpdate) {
    case "agent_message_chunk":
    case "agent_thought_chunk":
    case "tool_call":
    case "plan":
      return true;
    case "tool_call_update": {
      const tc = u as unknown as AcpToolCall & { _meta?: TerminalMeta | null };
      if (tc.status === "pending" || tc.status === "in_progress") return true;
      // Title/kind/content/rawOutput are real tool progress; terminal_* alone is the
      // background channel on an already-completed call.
      if (tc.title != null || tc.kind != null || tc.content != null || tc.rawOutput != null) {
        return true;
      }
      const meta = tc._meta;
      const terminalOnly =
        !!meta &&
        (meta.terminal_info != null ||
          meta.terminal_output != null ||
          meta.terminal_output_delta != null ||
          meta.terminal_exit != null);
      if (terminalOnly) {
        const call = prev ?? tc;
        return !(call.background || call.status === "completed");
      }
      return false;
    }
    default:
      return false;
  }
}

export function hasActiveForegroundTools(toolCalls: Record<string, AcpToolCall>): boolean {
  return Object.values(toolCalls).some(
    (tc) => !tc.background && (tc.status === "pending" || tc.status === "in_progress"),
  );
}

export function codeWorking(s: {
  busy: boolean;
  toolCalls: Record<string, AcpToolCall>;
  lastForegroundAt: number | null;
}, now = Date.now()): boolean {
  if (s.busy) return true;
  if (hasActiveForegroundTools(s.toolCalls)) return true;
  if (s.lastForegroundAt != null && now - s.lastForegroundAt < FOREGROUND_IDLE_MS) return true;
  return false;
}
