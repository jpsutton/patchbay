// Render check for the sidebar's session-usage gauge (pb-20261106-09): it renders the button and
// the pane the way a browser would and asserts what they show, so the contract — a gauge when the
// session has usage, nothing at all when it doesn't — can be re-run instead of taken on trust.
//
//   vite build --ssr src/dev/renderCheck.tsx --outDir node_modules/.cache/rendercheck --emptyOutDir
//   node node_modules/.cache/rendercheck/renderCheck.js [--dump]
//
// The outDir has to live inside the project so the bundle resolves react from node_modules. Nothing
// imports this file, so it never ships and never runs in the app.
import { renderToStaticMarkup } from "react-dom/server";
import { SessionUsageBlade, SessionUsageButton } from "../components/SessionUsage";
import { emptyParams, type CodeSession, type Model, type UsageView } from "../lib/ipc";
import { useStore } from "../lib/store";

// The app has no @types/node; this runs under plain node, not in the app.
declare const process: { argv: string[]; exit(code?: number): never };

const model: Model = {
  id: "m1",
  providerId: "p1",
  modelId: "demo-model",
  displayName: "Demo model",
  vision: false,
  contextWindow: 128_000,
  params: { ...emptyParams(), pricing: { input: 3, cachedInput: 0.3, cacheWrite: 3.75, output: 15 } },
};

const session = (agent: CodeSession["agent"]): CodeSession => ({
  id: "s1",
  cwd: "/home/me/src/patchbay",
  acpSessionId: null,
  title: "demo",
  modelRef: "m1",
  pinned: false,
  createdAt: 1,
  updatedAt: 1,
  activity: null,
  agent,
  agentModel: null,
  hostNode: null,
  epoch: 0,
  permissionMode: "default",
});

const usage: UsageView = {
  totals: { input: 41_000, cachedRead: 30_000, cacheWrite: 0, output: 1_800, reasoning: 0, costUsd: null, apiMs: 0 },
  contextTokens: 24_000,
  contextWindow: 128_000,
  autoCompactPercent: 85,
  cache: null,
};

const reportedUsage: UsageView = { ...usage, totals: { ...usage.totals, costUsd: 1.23 } };

const emptyUsage: UsageView = {
  totals: { input: 0, cachedRead: 0, cacheWrite: 0, output: 0, reasoning: 0, costUsd: null, apiMs: 0 },
  contextTokens: null,
  contextWindow: null,
  autoCompactPercent: null,
};

/**
 * Server rendering reads zustand through `getServerSnapshot`, which is the store's *initial*
 * state, so the seed writes there rather than through setState.
 */
function seed(u: UsageView | null, agent: CodeSession["agent"] = "patchbay", open = false) {
  const init = useStore.getInitialState() as unknown as Record<string, unknown>;
  init.mode = "code";
  init.activeCodeSessionId = "s1";
  init.codeSessions = [session(agent)];
  init.models = [model];
  init.providers = [];
  init.sessionUsageOpen = open;
  init.code = { ...(init.code as object), s1: { usage: u } };
}

const checks: { name: string; html: string; want: (h: string) => boolean }[] = [];

seed(usage);
checks.push({
  name: "gauge: patchbay session with usage",
  html: renderToStaticMarkup(<SessionUsageButton />),
  want: (h) =>
    h.includes("41k") && h.includes("~$0.07") && h.includes("<svg") && h.includes("Session usage: In 41,000 tokens"),
});

seed(reportedUsage);
checks.push({
  name: "gauge: provider-reported cost is not an estimate",
  html: renderToStaticMarkup(<SessionUsageButton />),
  want: (h) => h.includes("$1.23") && !h.includes("~$1.23") && h.includes("reported by provider"),
});

seed(null);
checks.push({
  name: "gauge: agent with no usage yet renders nothing (the old empty state)",
  html: renderToStaticMarkup(<SessionUsageButton />),
  want: (h) => h === "",
});

seed(emptyUsage);
checks.push({
  name: "gauge: usage reported but all zero renders nothing",
  html: renderToStaticMarkup(<SessionUsageButton />),
  want: (h) => h === "",
});

seed(usage, "opencode");
checks.push({
  name: "gauge: an agent that brings its own model is counted, not priced",
  html: renderToStaticMarkup(<SessionUsageButton />),
  // The host still reports a window, so the ring stays; only the price is missing.
  want: (h) => h.includes("41k") && !h.includes("$") && h.includes("<svg"),
});

seed(usage);
checks.push({
  name: "pane: closed renders nothing",
  html: renderToStaticMarkup(<SessionUsageBlade />),
  want: (h) => h === "",
});

seed(usage, "patchbay", true);
checks.push({
  name: "pane: open shows the section, the bar and the detail rows",
  html: renderToStaticMarkup(<SessionUsageBlade />),
  want: (h) =>
    h.includes("Session usage") &&
    h.includes("Demo model") &&
    h.includes("Cache write") &&
    h.includes("24,000 of 128,000") &&
    h.includes("~$0.07"),
});

seed(null, "patchbay", true);
checks.push({
  name: "pane: open but nothing reported renders nothing",
  html: renderToStaticMarkup(<SessionUsageBlade />),
  want: (h) => h === "",
});

let failed = 0;
for (const c of checks) {
  const ok = c.want(c.html);
  if (!ok) failed++;
  console.log(`${ok ? "ok  " : "FAIL"} ${c.name}${ok ? "" : `\n     got: ${c.html.slice(0, 400)}`}`);
}
console.log(`${checks.length - failed}/${checks.length} render checks passed`);
if (process.argv.includes("--dump")) {
  seed(usage);
  console.log("\n--- gauge ---\n" + renderToStaticMarkup(<SessionUsageButton />));
  seed(usage, "patchbay", true);
  console.log("\n--- pane ---\n" + renderToStaticMarkup(<SessionUsageBlade />));
}
if (failed) process.exit(1);
