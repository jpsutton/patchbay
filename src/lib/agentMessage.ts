/**
 * Agent-mesh messages (another Patchbay Code session) are delivered as user prompts
 * with an `origin` of `agent:<node>:<source_session_id>`. The UI renders them
 * distinctly from prompts David typed. Backward compatible: a missing/other origin
 * is a human prompt and renders exactly as before.
 */

export const AGENT_ORIGIN_PREFIX = "agent:";

export function isAgentOrigin(origin: string | undefined | null): origin is string {
  return typeof origin === "string" && origin.startsWith(AGENT_ORIGIN_PREFIX);
}

export interface AgentMessageLabel {
  /** The sending session's title, or null if it could not be parsed. */
  title: string | null;
  /** The sending node id, or null if it could not be parsed. */
  nodeId: string | null;
  /** A short, human-readable sender label. */
  label: string;
}

/**
 * Parse the sender label from the message text the backend builds, hoisting the
 * session title and node into the UI label. Format (see `deliver_agent_message_local`):
 *   "Patchbay agent message from Code session '<title>' (<id>) on <host> (<node>):"
 * Falls back to the origin-derived node when the text doesn't match (e.g. a
 * future/foreign format), so the bubble is still identifiable.
 */
export function parseAgentMessageLabel(text: string, origin: string | undefined | null): AgentMessageLabel {
  const header = text.split("\n", 1)[0] ?? "";
  const m = /^Patchbay agent message from Code session '([\s\S]*?)' \(([^)]+)\) on [\s\S]*? \(([^)]+)\):\s*$/.exec(header);
  if (m) {
    const title = m[1] || null;
    const nodeId = m[3] || null;
    return { title, nodeId, label: title ? `${title} · ${nodeId ?? "agent"}` : (nodeId ?? "Agent") };
  }
  const nodeId = isAgentOrigin(origin) ? origin.slice(AGENT_ORIGIN_PREFIX.length).split(":")[0] || null : null;
  return { title: null, nodeId, label: nodeId ? `Agent · ${nodeId}` : "Agent" };
}

/**
 * The message body for display: strips the backend's header line and the trailing
 * "To reply..." footer, leaving the actual message. Returns the original text when
 * the shape isn't recognized, so nothing is lost.
 */
export function agentMessageBody(text: string): string {
  if (!/^Patchbay agent message from /.test(text)) return text;
  const firstBlank = text.indexOf("\n\n");
  if (firstBlank < 0) return text;
  let body = text.slice(firstBlank + 2);
  const reply = body.lastIndexOf("\n\nTo reply, use the Patchbay send_agent_message tool");
  if (reply >= 0) body = body.slice(0, reply);
  return body.trimEnd() || text;
}
