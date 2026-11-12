/**
 * Run: node --experimental-strip-types src/lib/queueMirror.selftest.ts
 *
 * pb-20261107-30: the window's queue mirror.
 * 1. A rename leaves the queue alone — entries are id-keyed and the store's title branch
 *    writes only the title and the session list (pinned by source below).
 * 2. An `unqueued` event removes EVERY id the backend listed (the backend already decided
 *    they left): a peer keeps its place (never listed on a restore), and a prompt sent from
 *    ANOTHER computer's UI (origin "node-C") does not ghost in this window's queue.
 *    `handBack` is the only "mine only" filter: only this window's prompt reaches the
 *    composer; node-C's does not.
 * 3. A hard drop (!restore: user removal, MCP accept, pre-stop drain) removes everything
 *    listed — each of those having already told its sender.
 */
// @ts-expect-error — node:fs has no types in this app config (types: [vite/client]); this
// selftest runs under `node --experimental-strip-types`, not in the app bundle.
import { readFileSync } from "node:fs";
import { applyUnqueued, handBack } from "./queueMirror.ts";
import type { QueuedPrompt } from "./store";

function assert(cond: boolean, what: string) {
  if (!cond) throw new Error(`queueMirror selftest failed: ${what}`);
}

// (1) Rename keeps the queue: the title branch must never touch `queued` (id-keyed).
const storeSrc = readFileSync(new URL("./store.ts", import.meta.url), "utf8");
const titleAt = storeSrc.indexOf('if (e.type === "title")');
assert(titleAt >= 0, "the store has a title branch");
const titleBranch = storeSrc.slice(titleAt, storeSrc.indexOf("if (restart)", titleAt));
assert(!titleBranch.includes("queued"), `a title event must not touch the queue: ${titleBranch}`);
assert(titleBranch.includes("x.id === id"), "the title update is keyed by session id");

// (2) The backend's restore lists every non-agent id it removed — all leave the mirror;
// the agent-origin peer was never listed and keeps its place; node-C does not ghost, and
// only this window's prompt is handed back to the composer.
const queue: QueuedPrompt[] = [
  { promptId: "p1", text: "mine", attachments: [], origin: "node-A" },
  { promptId: "p2", text: "peer text", attachments: [], origin: "agent:node-B:sess-B" },
  { promptId: "p3", text: "from node-C's UI", attachments: [], origin: "node-C" },
];
const listedBySplit = ["p1", "p3"]; // what the backend's split emits: all non-agent ids it removed
const afterRestore = applyUnqueued(queue, listedBySplit);
assert(
  afterRestore.length === 1 && afterRestore[0].promptId === "p2",
  `the peer keeps its place and the listed ids leave, got ${JSON.stringify(afterRestore.map((q) => q.promptId))}`,
);
assert(
  !afterRestore.some((q) => q.origin === "node-C"),
  "a prompt from another computer's UI must not ghost in this window's queue",
);
const restored = handBack(
  null,
  queue.filter((q) => listedBySplit.includes(q.promptId)),
  "node-A",
);
assert(restored?.text === "mine", `only this window's prompt is handed back, got ${JSON.stringify(restored)}`);
assert(restored !== null && !restored.text.includes("node-C"), "node-C's prompt is not handed back");

// (3) A hard drop removes everything listed.
const afterDrop = applyUnqueued(queue, ["p2"]);
assert(afterDrop.length === 2 && !afterDrop.some((q) => q.promptId === "p2"), "the hard drop clears the listed entry");

console.log("queueMirror selftest: ok");
