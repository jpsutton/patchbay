/**
 * Run: node --experimental-strip-types src/lib/queueMirror.selftest.ts
 *
 * pb-20261107-30: the window's queue mirror.
 * 1. A rename leaves the queue alone — entries are id-keyed and the store's title branch
 *    writes only the title and the session list (pinned by source below).
 * 2. A restore-unqueue takes only THIS window's prompt back to the composer; an
 *    agent-origin entry keeps its place for the next turn.
 * 3. A hard drop (!restore: user removal, MCP accept, pre-stop drain) removes everything
 *    listed — each of those having already told its sender.
 */
// @ts-expect-error — node:fs has no types in this app config (types: [vite/client]); this
// selftest runs under `node --experimental-strip-types`, not in the app bundle.
import { readFileSync } from "node:fs";
import { applyUnqueued } from "./queueMirror.ts";

function assert(cond: boolean, what: string) {
  if (!cond) throw new Error(`queueMirror selftest failed: ${what}`);
}

const queue = [
  { promptId: "p1", text: "mine", origin: "node-A" },
  { promptId: "p2", text: "peer text", origin: "agent:node-B:sess-B" },
];

// (1) Rename keeps the queue: the title branch must never touch `queued` (id-keyed).
const storeSrc = readFileSync(new URL("./store.ts", import.meta.url), "utf8");
const titleAt = storeSrc.indexOf('if (e.type === "title")');
assert(titleAt >= 0, "the store has a title branch");
const titleBranch = storeSrc.slice(titleAt, storeSrc.indexOf("if (restart)", titleAt));
assert(!titleBranch.includes("queued"), `a title event must not touch the queue: ${titleBranch}`);
assert(titleBranch.includes("x.id === id"), "the title update is keyed by session id");

// (2) A restore-unqueue takes only this window's prompt back; the peer keeps its place.
const afterRestore = applyUnqueued(queue, { promptIds: ["p1", "p2"], restore: true }, "node-A");
assert(
  afterRestore.length === 1 && afterRestore[0].promptId === "p2",
  `the peer keeps its place, got ${JSON.stringify(afterRestore.map((q) => q.promptId))}`,
);

// (3) A hard drop removes everything listed.
const afterDrop = applyUnqueued(queue, { promptIds: ["p2"], restore: false }, "node-A");
assert(afterDrop.length === 1 && afterDrop[0].promptId === "p1", "only the listed entry drops");

console.log("queueMirror selftest: ok");
