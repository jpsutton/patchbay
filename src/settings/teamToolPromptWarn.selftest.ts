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

// always-approve: no warn (David: warn, allow always-approve).
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "always-approve", prompts: ["opencode"], label }) === null,
  "always-approve suppresses warn",
);

// Workers / non-standing: no warn from this helper.
assert(
  teamToolPromptWarn({ standing: false, permissionMode: "default", prompts: ["opencode"], label }) === null,
  "non-standing suppressed",
);

// Agents with a team-tool rule: empty prompts → no warn.
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "default", prompts: [], label }) === null,
  "empty prompts",
);
assert(
  teamToolPromptWarn({ standing: true, permissionMode: "default", prompts: undefined, label }) === null,
  "undefined prompts",
);

console.log("teamToolPromptWarn selftest: ok");
