import type { TeamDecision } from "../lib/ipc";
import { splitTeamDecisions } from "./teamBoardDecisions.ts";

function expectIds(actual: TeamDecision[], expected: string[]) {
  const ids = actual.map((d) => d.id);
  if (ids.length !== expected.length || ids.some((id, i) => id !== expected[i])) {
    throw new Error(`expected [${expected.join(", ")}], got [${ids.join(", ")}]`);
  }
}

const decision = (id: string, status: TeamDecision["status"]): TeamDecision => ({
  teamId: "team", id, itemId: null, question: id, options: [], recommendation: null, class: "advisor_first",
  openedBy: null, advisorVerdict: null, advisorChoice: null, advisorText: null, userAnswer: null, status,
  disagrees: false, createdAt: 0, resolvedAt: null,
});

const split = splitTeamDecisions([
  decision("advisor", "awaiting_advisor"),
  decision("user", "awaiting_user"),
  decision("cleared", "cleared"),
  decision("resolved", "resolved"),
]);
expectIds(split.waiting, ["advisor", "user"]);
expectIds(split.other, ["cleared", "resolved"]);
console.log("teamBoardDecisions selftest: ok");
