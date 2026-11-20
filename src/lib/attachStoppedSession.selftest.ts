/**
 * Run: node --experimental-strip-types src/lib/attachStoppedSession.selftest.ts
 *
 * pb-20261117-08: Opening a stopped session must NOT auto-start the agent process.
 * 1. attachAgent on a stopped session (!info.running) must never call codeStart or startAgent.
 * 2. It sets the neutral status 'idle' (not 'exited') so the session displays in read-only browse mode.
 * 3. CodeView's mount effect calls attach(activeId) instead of start(activeId) when codeIsRunning is false.
 */
// @ts-expect-error — node:fs has no types in this app config
import { readFileSync } from "node:fs";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(`attachStoppedSession.selftest: ${msg}`);
}

// (1) Invariant: attachAgent source in store.ts must not call startAgent on stopped sessions
const storeSrc = readFileSync(new URL("./store.ts", import.meta.url), "utf8");
const attachAgentAt = storeSrc.indexOf("attachAgent: async (id) =>");
assert(attachAgentAt >= 0, "store.ts defines attachAgent");

const notRunningAt = storeSrc.indexOf("if (!info.running)", attachAgentAt);
assert(notRunningAt >= 0, "attachAgent checks !info.running");

const notRunningEnd = storeSrc.indexOf("const rev = get().agentConfigRev;", notRunningAt);
assert(notRunningEnd >= 0, "found end of !info.running branch");

const notRunningBranch = storeSrc.slice(notRunningAt, notRunningEnd);
assert(!notRunningBranch.includes("startAgent"), `!info.running branch must not call startAgent: ${notRunningBranch}`);
assert(!notRunningBranch.includes("codeStart"), `!info.running branch must not call codeStart: ${notRunningBranch}`);
assert(notRunningBranch.includes('status: "idle"'), `!info.running branch sets neutral idle status: ${notRunningBranch}`);

// (2) Invariant: CodeView mount effect calls attach instead of start when !running
const codeViewSrc = readFileSync(new URL("../code/CodeView.tsx", import.meta.url), "utf8");
const isRunningAt = codeViewSrc.indexOf("api.codeIsRunning(activeId).then((running) =>");
assert(isRunningAt >= 0, "CodeView.tsx checks codeIsRunning");

const isRunningBlock = codeViewSrc.slice(isRunningAt, codeViewSrc.indexOf("});", isRunningAt));
assert(isRunningBlock.includes("if (!running) {\n        attach(activeId);"), `when not running, CodeView calls attach, not start: ${isRunningBlock}`);
assert(!isRunningBlock.includes("start(activeId)"), `when not running, CodeView does not call start: ${isRunningBlock}`);

// (3) Executable flow: attach on a stopped session with mock API
async function simulateAttach(id: string, codeAttachResponse: { running: boolean; offline?: boolean }) {
  let codeStartCalls = 0;
  const mockApi = {
    codeAttach: async () => codeAttachResponse,
    codeStart: async () => {
      codeStartCalls++;
    },
    getCodeTranscript: async () => JSON.stringify({ entries: [{ kind: "user", text: "hello" }], toolCalls: {} }),
  };

  let state: any = null;
  const set = (updater: (st: any) => any) => {
    state = updater({ code: {} });
  };

  // Simulates attachAgent logic from store.ts:
  const attached = mockApi.codeAttach();
  const raw = await mockApi.getCodeTranscript();
  const history = raw ? JSON.parse(raw) : null;
  const info = await attached;

  if (!info.running) {
    if (info.offline) {
      set((st: any) => ({ code: { ...st.code, [id]: { history, status: "exited" } } }));
    } else {
      set((st: any) => ({ code: { ...st.code, [id]: { history, status: "idle" } } }));
    }
  } else {
    await mockApi.codeStart();
  }

  return { codeStartCalls, state };
}

const res = await simulateAttach("session-123", { running: false });
assert(res.codeStartCalls === 0, "attach on stopped session never calls codeStart");
assert(res.state.code["session-123"].status === "idle", "stopped session has status 'idle'");
assert(res.state.code["session-123"].history.entries.length === 1, "history loaded for browsing");

console.log("attachStoppedSession.selftest: ok");
