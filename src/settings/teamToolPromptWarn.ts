/**
 * Seats whose agents lack an allow.rs team-tool rule (`Seat.prompts`, via `!has_rule`):
 * - Standing: need a human unless always-approve.
 * - Workers: cannot use always-approve, so they always wait on a human for team tools.
 * Warn only — never block save/start.
 */
export function teamToolPromptWarn(args: {
  standing: boolean;
  permissionMode: string;
  /** Agents from the backend whose team tools still prompt (`!has_rule`). */
  prompts: string[] | undefined;
  label?: (agent: string) => string;
}): string | null {
  const agents = args.prompts ?? [];
  if (agents.length === 0) return null;
  const label = args.label ?? ((a: string) => a);
  const names = agents.map(label).join(", ");
  if (args.standing) {
    if (args.permissionMode === "always-approve") return null;
    return `${names}: this agent needs a human answer for every team tool unless the seat runs in always-approve.`;
  }
  // Worker seats: always-approve is refused elsewhere; no escape hatch.
  return `${names}: this agent needs a human answer for every team tool call; worker seats cannot use always-approve, so this seat will always wait on a human`;
}
