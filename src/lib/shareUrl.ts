/** A Phase 2 artifact share link: `{base}/share/{nodeId}/{token}` (see src-tauri/shares.rs). */
export interface ShareRef {
  nodeId: string;
  token: string;
  /** The original URL, kept for the "open in browser" fallback. */
  url: string;
}

/**
 * Recognize a mesh share link in chat. The node id is a 32-char hex mesh id and the token a
 * 64-char hex capability; being strict keeps unrelated http(s) links out of the in-app viewer.
 * Returns null for ordinary links, legacy Phase-1 `/share/{token}` links, and malformed ones.
 */
export function parseShareUrl(href: string | undefined | null): ShareRef | null {
  if (!href) return null;
  let u: URL;
  try {
    u = new URL(href);
  } catch {
    return null;
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") return null;
  const m = /\/share\/([^/]+)\/([^/]+)\/?$/.exec(u.pathname);
  if (!m) return null;
  const [, nodeId, token] = m;
  if (!/^[0-9a-f]{32}$/i.test(nodeId) || !/^[0-9a-f]{64}$/i.test(token)) return null;
  return { nodeId, token, url: href };
}
