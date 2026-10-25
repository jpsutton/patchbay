/**
 * Run: node --experimental-strip-types src/lib/builtinVoice.selftest.ts
 */
import { renderSpeech } from "./builtinVoice.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const hello = renderSpeech("Hello. This is Patchbay.", "normal");
assert(hello.length > 1000, "hello produces samples");
let energy = 0;
for (let i = 0; i < hello.length; i++) energy += hello[i] * hello[i];
assert(energy > 1, `hello is audible, energy ${energy}`);

const slow = renderSpeech("Hello.", "slower");
const fast = renderSpeech("Hello.", "faster");
assert(slow.length > fast.length, "slower is longer than faster");
assert(renderSpeech("   ", "normal").length === 0, "blank text is silent");

console.log("builtinVoice.selftest: ok");
