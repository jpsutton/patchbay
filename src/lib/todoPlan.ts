import type { AcpPlanEntry, AcpToolCall } from "./ipc";

/**
 * OpenCode and Kilo (its fork) send no ACP `plan`; their to-do list is the `todowrite` tool. A
 * live write is titled `todowrite` until it completes as "N todos" with the saved list in
 * `rawOutput.metadata.todos`; `rawInput.todos` before that is only what the agent proposed, and a
 * rejected write still sends it before failing. A `session/load` replay sends each past write
 * titled "N todos" with the list in `rawInput`.
 *
 * The plan a tool call stored: its list when it's a completed to-do write whose list reads, else
 * null. An empty list is a plan too: it clears the old one.
 */
export function todoPlanOf(tc: AcpToolCall): AcpPlanEntry[] | null {
  if (tc.status !== "completed" || !tc.title || !/^(todowrite|\d+ todos?)$/.test(tc.title)) return null;
  const saved = ((tc.rawOutput as { metadata?: { todos?: unknown } } | null)?.metadata ?? {}).todos;
  const todos = saved ?? (tc.rawInput as { todos?: unknown } | null)?.todos;
  if (!Array.isArray(todos)) return null;
  const out: AcpPlanEntry[] = [];
  for (const t of todos) {
    if (!t || typeof t !== "object" || typeof (t as { content?: unknown }).content !== "string") return null;
    const { content, status, priority } = t as { content: string; status?: unknown; priority?: unknown };
    out.push({
      content,
      status: status === "completed" || status === "in_progress" || status === "cancelled" ? status : "pending",
      priority: typeof priority === "string" ? priority : "medium",
    });
  }
  return out;
}
