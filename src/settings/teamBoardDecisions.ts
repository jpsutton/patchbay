import type { TeamDecision } from "../lib/ipc";

export function splitTeamDecisions(decisions: TeamDecision[]) {
  return {
    waiting: decisions.filter((d) => d.status === "awaiting_user" || d.status === "awaiting_advisor"),
    other: decisions.filter((d) => d.status !== "awaiting_user" && d.status !== "awaiting_advisor"),
  };
}
