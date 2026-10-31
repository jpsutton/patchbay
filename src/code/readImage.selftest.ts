/**
 * Pure-function checks for spotting a read of an image file.
 * Run: node --experimental-strip-types src/code/readImage.selftest.ts
 */
import type { AcpToolCall } from "../lib/ipc";
import { readImagePath } from "./readImage.ts";

function eq(actual: unknown, expected: unknown, msg: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}

const call = (over: Partial<AcpToolCall>): AcpToolCall => ({ toolCallId: "t1", kind: "read", status: "completed", ...over });

// The location wins; the raw input is the fallback.
eq(readImagePath(call({ locations: [{ path: "/w/shot.png" }] })), "/w/shot.png", "location");
eq(readImagePath(call({ rawInput: { file_path: "/w/a.JPG" } })), "/w/a.JPG", "file_path, upper case");
eq(readImagePath(call({ rawInput: { path: "docs/b.webp" } })), "docs/b.webp", "relative path");
eq(readImagePath(call({ locations: [], rawInput: { filePath: "c.jpeg" } })), "c.jpeg", "camel case");
eq(readImagePath(call({ locations: [{ path: "/w/src/main.ts" }], rawInput: { path: "/w/d.gif" } })), "/w/d.gif", "skips a non-image location");

// Not an image file, not a read, or a read that failed.
eq(readImagePath(call({ locations: [{ path: "/w/README.md" }] })), null, "text file");
eq(readImagePath(call({ locations: [{ path: "/w/icon.svg" }] })), null, "svg is text");
eq(readImagePath(call({ kind: "edit", locations: [{ path: "/w/shot.png" }] })), null, "edit");
eq(readImagePath(call({ status: "failed", locations: [{ path: "/w/shot.png" }] })), null, "failed read");
eq(readImagePath(call({ rawInput: "cat shot.png" })), null, "string input");
eq(readImagePath(call({})), null, "nothing");

console.log("readImage selftest ok");
