import type { AcpToolCall } from "../lib/ipc";

/**
 * The agent's own plain-English line for a tool call: the `description` field in its input
 * (Claude Code's Bash and Task tools send one). First line only; null when there is none.
 */
export function toolDescription(tc: Pick<AcpToolCall, "rawInput">): string | null {
  const raw = tc.rawInput;
  if (!raw || typeof raw !== "object") return null;
  const d = (raw as Record<string, unknown>).description;
  if (typeof d !== "string") return null;
  const line = d.trim().split("\n")[0].trim();
  return line || null;
}
