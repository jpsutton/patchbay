/** The worker-start refusal follows the ordinary error entry path. */
// @ts-expect-error node:fs is not part of the app tsconfig
import { readFileSync } from "node:fs";
function assert(ok: unknown, msg: string): asserts ok { if (!ok) throw new Error(msg); }
const store = readFileSync(new URL("../lib/store.ts", import.meta.url), "utf8");
const view = readFileSync(new URL("./CodeView.tsx", import.meta.url), "utf8");
const manager = readFileSync(new URL("../../src-tauri/src/teams/dispatch.rs", import.meta.url), "utf8");
assert(manager.includes("worker cannot restart in the same worktree"), "worker refusal must be clear");
assert(store.includes('applyCodeEvent(id, { type: "error", message: errorText(e) })'), "start errors reach Code events");
assert(store.includes('{ kind: "error", text: e.message }'), "error event enters the transcript");
assert(/case "error":[\s\S]*?\{e\.text\}/.test(view), "the error entry renders its text");
assert(!view.includes("workerRestartConfirm"), "old confirmation is gone");
console.log("worker restart refusal rendering selftest: ok");
