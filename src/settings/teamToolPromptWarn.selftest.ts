import { teamToolPromptWarn } from "./teamToolPromptWarn.ts";

const label = (a: string) => ({ opencode: "OpenCode", grok: "Grok", kilo: "Kilo" }[a] ?? a);

function assert(cond: unknown, msg: string) {
  if (!cond) throw new Error(`teamToolPromptWarn.selftest: ${msg}`);
}

// Standing + no-rule agents + default/auto → warn (list comes from backend has_rule, not hard-coded here).
const warn = teamToolPromptWarn({
  standing: true,
  permissionMode: "default",
  prompts: ["opencode", "grok"],
  label,
});
assert(warn?.includes("OpenCode"), "labels OpenCode");
assert(warn?.includes("Grok"), "labels Grok");
assert(warn?.includes("always-approve"), "mentions always-approve escape");
assert(warn?.includes("human answer"), "mentions human answer");

assert(
  teamToolPromptWarn({ standing: true, permissionMode: "auto", prompts: ["kilo"], label })?.includes("Kilo"),
  "auto mode still warns",
);

// always-approve: no warn for standing (David: warn, allow always-approve).
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "always-approve", prompts: ["opencode"], label }) === null,
  "always-approve suppresses standing warn",
);

// Workers / non-standing with no-rule agent: always warn (cannot use always-approve).
const workerWarn = teamToolPromptWarn({
  standing: false,
  permissionMode: "default",
  prompts: ["opencode"],
  label,
});
assert(workerWarn?.includes("OpenCode"), "worker labels OpenCode");
assert(workerWarn?.includes("worker seats cannot use always-approve"), "worker names the constraint");
assert(workerWarn?.includes("always wait on a human"), "worker says always waits");
assert(
  teamToolPromptWarn({ standing: false, permissionMode: "auto", prompts: ["opencode"], label })?.includes(
    "worker seats cannot use always-approve",
  ),
  "worker auto still warns",
);
assert(
  teamToolPromptWarn({ standing: false, permissionMode: "always-approve", prompts: ["opencode"], label })?.includes(
    "worker seats cannot use always-approve",
  ),
  "worker warns even if mode were always-approve",
);

// Agents with a team-tool rule: empty prompts → no warn.
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "default", prompts: [], label }) === null,
  "empty prompts standing",
);
assert(
  teamToolPromptWarn({ standing: false, permissionMode: "default", prompts: [], label }) === null,
  "empty prompts worker",
);
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "default", prompts: undefined, label }) === null,
  "undefined prompts",
);

console.log("teamToolPromptWarn selftest: ok");
