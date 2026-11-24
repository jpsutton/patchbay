/**
 * Run: node --experimental-strip-types src/lib/pathOf.selftest.ts
 */
import { fullPathOn, pathOf } from "./pathOf.ts";

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

// A move's folder in the target computer's form (pb-20261124-10).
assert(fullPathOn(String.raw`C:\Users\me\repo`, "macos") === false, "a Windows folder is relative on macOS");
assert(fullPathOn("/Users/me/repo", "windows") === false, "a macOS folder is relative on Windows");
assert(fullPathOn("/Users/me/repo", "macos") === true, "macOS full path");
assert(fullPathOn("/home/me/repo", "linux") === true, "Linux full path");
assert(fullPathOn(String.raw`C:\Users\me\repo`, "windows") === true, "Windows drive path");
assert(fullPathOn("D:/repo", "windows") === true, "Windows drive path, forward slash");
assert(fullPathOn(String.raw`\\host\share\repo`, "windows") === true, "UNC is full (refused later as a share)");
assert(fullPathOn("~/repo", "macos") === false, "a tilde is not expanded by the target");
assert(fullPathOn("repo", "linux") === false, "bare name");
assert(fullPathOn(String.raw`C:\Users\me\repo`, null) === null, "unknown OS: no verdict");
assert(fullPathOn("/Users/me/repo", undefined) === null, "unknown OS: no verdict");

console.log("pathOf.selftest: ok");
