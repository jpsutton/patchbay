/**
 * Standing seats that call team tools: agents without an allow.rs team-tool rule
 * (`Seat.prompts`, filled via `has_rule`) need a human on every call unless the seat
 * is always-approve. Warn only — never block save/start.
 */
export function teamToolPromptWarn(args: {
  standing: boolean;
  permissionMode: string;
  /** Agents from the backend whose team tools still prompt (`!has_rule`). */
  prompts: string[] | undefined;
  label?: (agent: string) => string;
}): string | null {
  if (!args.standing) return null;
  if (args.permissionMode === "always-approve") return null;
  const agents = args.prompts ?? [];
  if (agents.length === 0) return null;
  const label = args.label ?? ((a: string) => a);
  const names = agents.map(label).join(", ");
  return `${names}: this agent needs a human answer for every team tool unless the seat runs in always-approve.`;
}
