/**
 * Run: node --experimental-strip-types src/lib/shareUrl.selftest.ts
 */
import { parseShareUrl } from "./shareUrl.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const NODE = "a".repeat(32);
const TOKEN = "b".repeat(64);

// A Phase 2 link: node id in the path, raw gateway base, or a path-prefixed gateway base.
const plain = `http://127.0.0.1:4420/share/${NODE}/${TOKEN}`;
assert(parseShareUrl(plain)?.nodeId === NODE, "node id parsed");
assert(parseShareUrl(plain)?.token === TOKEN, "token parsed");
assert(parseShareUrl(plain)?.url === plain, "original url kept for the browser fallback");
const prefixed = `https://gateway.example/patchbay/share/${NODE}/${TOKEN}?x=1`;
assert(parseShareUrl(prefixed)?.token === TOKEN, "path prefix and query are fine");

// Ordinary links, legacy Phase-1 single-segment links, and malformed ids are NOT hijacked.
assert(parseShareUrl("https://example.com/page") === null, "ordinary link");
assert(parseShareUrl(`http://host/share/${TOKEN}`) === null, "legacy one-segment link");
assert(parseShareUrl(`http://host/share/nothex/${TOKEN}`) === null, "bad node id");
assert(parseShareUrl(`http://host/share/${NODE}/short`) === null, "bad token");
assert(parseShareUrl(`http://host/share/${NODE}/${TOKEN}/more`) === null, "too many segments");
assert(parseShareUrl("mailto:x@y.z") === null, "mailto");
assert(parseShareUrl("not a url") === null, "garbage");
assert(parseShareUrl(undefined) === null, "missing");

console.log("shareUrl.selftest: ok");
