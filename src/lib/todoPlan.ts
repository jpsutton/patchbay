import type { AcpPlanEntry, AcpToolCall, AgentId } from "./ipc";

/** The agents whose `todowrite` tool is their plan. */
const TODO_AGENTS: readonly AgentId[] = ["opencode", "kilo"];

/**
 * OpenCode and Kilo (its fork) send no ACP `plan`; their to-do list is the `todowrite` tool. A
 * live write is titled `todowrite` until it completes as "N todos" with the saved list in
 * `rawOutput.metadata.todos`; `rawInput.todos` before that is only what the agent proposed, and a
 * rejected write still sends it before failing. A `session/load` replay sends each past write
 * titled "N todos" with the list in `rawInput`.
 *
 * The plan a tool call stored: its list when `agent` is OpenCode or Kilo and the call is a
 * completed to-do write whose list reads, else null. Other agents' tools may share the title, so
 * the agent gates it. An empty list is a plan too: it clears the old one.
 */
export function todoPlanOf(tc: AcpToolCall, agent: AgentId | undefined): AcpPlanEntry[] | null {
  if (!agent || !TODO_AGENTS.includes(agent)) return null;
  if (tc.status !== "completed" || !tc.title || !/^(todowrite|\d+ todos?)$/.test(tc.title)) return null;
  const todos = savedTodos(tc) ?? (tc.rawInput as { todos?: unknown } | null)?.todos;
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

/** The list a completed live write saved (`rawOutput.metadata.todos`), if it carries one. */
export function savedTodos(tc: AcpToolCall | undefined): unknown {
  return ((tc?.rawOutput as { metadata?: { todos?: unknown } } | null)?.metadata ?? {}).todos;
}

/**
 * The plan a tool call update sets, from the call before (`prev`) and after (`merged`) it: only
 * the frame that completes a write (or a replay announcing it completed), or one that first
 * brings its saved list. A late repeat of an older write can't bring back its list over a newer
 * one.
 */
export function planUpdateOf(
  prev: AcpToolCall | undefined,
  merged: AcpToolCall,
  agent: AgentId | undefined,
): AcpPlanEntry[] | null {
  const settles =
    (merged.status === "completed" && prev?.status !== "completed") ||
    (savedTodos(merged) !== undefined && savedTodos(prev) === undefined);
  return settles ? todoPlanOf(merged, agent) : null;
}
