/**
 * Run: node --experimental-strip-types src/lib/agentMessage.selftest.ts
 *
 * Proves agent-mesh origin detection and label/body parsing. A non-agent (human)
 * prompt must be unaffected; an agent prompt must be recognized and labeled.
 */
import { agentMessageBody, isAgentOrigin, parseAgentMessageLabel } from "./agentMessage.ts";

function assert(cond: boolean, what: string) {
  if (!cond) throw new Error(`agentMessage selftest failed: ${what}`);
}

// Human prompts: no origin, empty, or unrelated origin — never treated as agent.
assert(!isAgentOrigin(undefined), "undefined is human");
assert(!isAgentOrigin(null), "null is human");
assert(!isAgentOrigin(""), "empty is human");
assert(!isAgentOrigin("local"), "local is human");
assert(!isAgentOrigin("me"), "me is human");
assert(!isAgentOrigin("user:123"), "other prefix is human");

// Agent origin.
assert(isAgentOrigin("agent:node-a:session-b"), "agent origin recognized");
assert(isAgentOrigin("agent:"), "bare agent prefix recognized");

// Label/body parsing from the real backend format.
const origin = "agent:node-x:4e16019e-c89c-4d2d-a302-6e899cafa78b";
const text =
  "Patchbay agent message from Code session 'Triage Desk - OpenCode' (4e16019e-c89c-4d2d-a302-6e899cafa78b) on david-mac (node-x):\n\n" +
  "Please investigate the crash.\n\n" +
  "To reply, use the Patchbay send_agent_message tool and target session 4e16019e-c89c-4d2d-a302-6e899cafa78b. Do not resend just because a prior delivery was queued or steered — that means it was accepted. Only message them when you have new information.";

const info = parseAgentMessageLabel(text, origin);
assert(info.title === "Triage Desk - OpenCode", `title parsed, got ${info.title}`);
assert(info.nodeId === "node-x", `node parsed, got ${info.nodeId}`);
assert(info.label.includes("Triage Desk - OpenCode"), "label carries title");
assert(info.label.includes("node-x"), "label carries node");
assert(agentMessageBody(text) === "Please investigate the crash.", `body hoisted, got: ${agentMessageBody(text)}`);

// Unrecognized format falls back safely and never drops text.
const weird = parseAgentMessageLabel("hello", origin);
assert(weird.title === null, "no title on weird format");
assert(weird.nodeId === "node-x", "node falls back to origin");
assert(weird.label.includes("Agent"), "label still identifies agent");
assert(agentMessageBody("hello") === "hello", "unknown format keeps full text");

// Human text is never mangled by the body helper.
assert(agentMessageBody("plain human message") === "plain human message", "human text untouched");

console.log("agentMessage selftest ok");
