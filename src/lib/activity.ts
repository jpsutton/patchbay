import type { AcpToolCall } from "./ipc";

/**
 * What a Code session has done, strongest first: opened a pull request, created a branch,
 * committed, edited files, or only talked. Drives the session's sidebar icon.
 */
export type Activity = "pr" | "branch" | "commit" | "edit" | "chat";

const RANK: Record<Activity, number> = { chat: 0, edit: 1, commit: 2, branch: 3, pr: 4 };

export function strongest(a: Activity | null | undefined, b: Activity | null | undefined): Activity | null {
  if (!a) return b ?? null;
  if (!b) return a;
  return RANK[a] >= RANK[b] ? a : b;
}

const PR = /\bgh\s+pr\s+create\b|create_pull_request/;
const BRANCH = /\bgit\s+(?:checkout\s+-[bB]|switch\s+(?:-c|-C|--create)|branch\s+(?!-)[\w./-]+|worktree\s+add)\b/;
const COMMIT = /\bgit\s+commit\b/;
const EDIT_KINDS = new Set(["edit", "delete", "move"]);

function commandText(tc: AcpToolCall): string {
  const raw = tc.rawInput as { command?: unknown; cmd?: unknown } | null | undefined;
  const cmd = raw && typeof raw === "object" ? (raw.command ?? raw.cmd) : undefined;
  const parts = [tc.title ?? ""];
  if (typeof cmd === "string") parts.push(cmd);
  else if (Array.isArray(cmd)) parts.push(cmd.join(" "));
  return parts.join("\n");
}

function classify(tc: AcpToolCall): Activity {
  // Only finished calls count: a pending one may still be denied, a failed one changed nothing.
  if (tc.status !== "completed") return "chat";
  const text = commandText(tc);
  if (PR.test(text)) return "pr";
  if (tc.kind === "execute" || tc.kind === "other" || !tc.kind) {
    if (BRANCH.test(text)) return "branch";
    if (COMMIT.test(text)) return "commit";
  }
  if ((tc.kind && EDIT_KINDS.has(tc.kind)) || tc.content?.some((c) => c.type === "diff")) return "edit";
  return "chat";
}

export function activityOf(toolCalls: Iterable<AcpToolCall>): Activity {
  let best: Activity = "chat";
  for (const tc of toolCalls) best = strongest(best, classify(tc))!;
  return best;
}
