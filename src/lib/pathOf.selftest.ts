/**
 * Run: node --experimental-strip-types src/lib/pathOf.selftest.ts
 */
import { pathOf } from "./pathOf.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

// Network locations (TEST-NET-3 host) link, so a click can copy them.
assert(pathOf(String.raw`\\203.0.113.1\s\x`) === String.raw`\\203.0.113.1\s\x`, "UNC backslash");
assert(pathOf("//203.0.113.1/s/x") === "//203.0.113.1/s/x", "UNC slash");
assert(pathOf(String.raw`\\203.0.113.1\s\x.txt`) !== null, "UNC with extension");
assert(pathOf(String.raw`\/203.0.113.1/s/x`) !== null, "UNC mixed separators");
assert(pathOf(String.raw`\\host\share\dir\file.txt:12`) === String.raw`\\host\share\dir\file.txt`, "UNC keeps the line split");
// Not network locations.
assert(pathOf("//TODO") === null, "a comment marker is not a share");
assert(pathOf(String.raw`\\host`) === null, "a host with no share");
assert(pathOf("https://x/y") === null, "a URL is not a path");
assert(pathOf("smb://203.0.113.1/s/x") === null, "an smb URL is not a path");
// Unchanged.
assert(pathOf("C:\\Users\\x\\a.txt") === "C:\\Users\\x\\a.txt", "drive path");
assert(pathOf("~/a/b.md") === "~/a/b.md", "tilde path");
assert(pathOf("/usr/bin/env") === "/usr/bin/env", "absolute path");
assert(pathOf("./a/b") === "./a/b", "relative path");
assert(pathOf("src/a.ts:12:4") === "src/a.ts", "line reference");
assert(pathOf("foo.bar()") === null, "code stays plain");
assert(pathOf("a = b") === null, "space stays plain");
assert(pathOf("1.2.3") === null, "version stays plain");
assert(pathOf("..") === null, "dots stay plain");

console.log("pathOf.selftest: ok");
