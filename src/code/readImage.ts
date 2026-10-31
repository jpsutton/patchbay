import type { AcpToolCall } from "../lib/ipc";

/** Image types the host can send back; mirrors `files::session_image` in the backend. */
const IMAGE_FILE = /\.(png|jpe?g|gif|webp|bmp)$/i;

/** Input fields agents use for the file a read tool opens. */
const PATH_KEYS = ["file_path", "filePath", "path", "target_file"];

/**
 * The image file a read tool call opened, or null when it read something else. The path is the
 * one the agent used: absolute, or relative to the session folder.
 */
export function readImagePath(tc: AcpToolCall): string | null {
  if (tc.kind !== "read" || tc.status === "failed") return null;
  const raw = tc.rawInput && typeof tc.rawInput === "object" ? (tc.rawInput as Record<string, unknown>) : {};
  const candidates = [
    ...(tc.locations ?? []).map((l) => l.path),
    ...PATH_KEYS.map((k) => raw[k]),
  ];
  const hit = candidates.find((p): p is string => typeof p === "string" && IMAGE_FILE.test(p.trim()));
  return hit ? hit.trim() : null;
}
