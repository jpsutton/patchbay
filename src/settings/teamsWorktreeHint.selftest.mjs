// F8: Worktree folder field must tell the user that leaving it empty uses <team folder>/.trees.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const page = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "TeamsPage.tsx"), "utf8");
if (!page.includes("Leave empty to use <team folder>/.trees")) {
  console.error("teamsWorktreeHint.selftest: TeamsPage Worktree folder hint missing 'Leave empty to use'");
  process.exit(1);
}
console.log("teamsWorktreeHint selftest: ok");
