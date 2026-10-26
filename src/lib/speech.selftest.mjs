/** Run: node src/lib/speech.selftest.mjs */
import assert from "node:assert/strict";
import { createServer } from "vite";

let lists = 0;
let speaks = [];
let stops = 0;
let listError = null;
let complete;
const prefs = { readAloudEngine: "auto", readAloudVoice: "pixel", readAloudRate: "normal" };
globalThis.__speechTest = {
  prefs,
  api: {
    listNativeVoices: async () => { lists++; if (listError) throw listError; return [{ id: "pixel", name: "Pixel voice" }]; },
    speakNative: (text, voice, rate) => { speaks.push({ text, voice, rate }); return new Promise((r) => { complete = r; }); },
    stopNative: async () => { stops++; complete?.(); },
  },
};
globalThis.window = {
  __PATCHBAY_REMOTE__: true,
  __TAURI_INTERNALS__: {},
  speechSynthesis: { getVoices: () => { throw new Error("Android must not probe Web Speech"); }, cancel() {} },
  setTimeout, setInterval, clearInterval,
};
Object.defineProperty(globalThis, "navigator", { value: { userAgent: "Android Pixel" }, configurable: true });
const server = await createServer({
  configFile: false,
  server: { middlewareMode: true },
  plugins: [{
    name: "speech-test-dependencies",
    enforce: "pre",
    resolveId(id, importer) {
      if (importer?.endsWith("/speech.ts") && ["./ipc", "./store", "./shortcuts"].includes(id)) return `\0speech-test:${id}`;
    },
    load(id) {
      if (id === "\0speech-test:./ipc") return "export const api = globalThis.__speechTest.api";
      if (id === "\0speech-test:./store") return "export const useStore = { getState: () => ({ prefs: globalThis.__speechTest.prefs }) }";
      if (id === "\0speech-test:./shortcuts") return "export const isMac = false";
    },
  }],
});
const turn = () => new Promise((r) => setImmediate(r));
try {
  const speech = await server.ssrLoadModule("/src/lib/speech.ts");
  assert.equal((await speech.systemVoices())[0].source, "native");
  await speech.systemVoices();
  assert.equal(lists, 2, "Android refreshes voices after downloads/settings changes");
  const first = speech.speakText("First sentence.", "one");
  await turn();
  assert.equal(speaks.length, 1);
  assert.deepEqual(speaks[0], { text: "First sentence.", voice: "pixel", rate: "normal" });
  const second = speech.enqueueSpeech("Second sentence.", "two");
  await turn();
  assert.equal(speaks.length, 1, "second reply waits for native completion");
  complete();
  await first; await turn();
  assert.equal(speaks.length, 2);
  speech.stopSpeech(); await second;
  assert.equal(speech.speakingNow(), null);
  assert.ok(stops >= 2, "Stop reaches Android");
  listError = new Error("Android voice data missing");
  await assert.rejects(speech.speakText("Fail visibly.", "error"), /voice data missing/);
  assert.equal(speech.speakingNow(), null);
  assert.equal(speaks.length, 2, "native failure never silently falls back");

  // Explicit Built-in remains usable even with a broken Android engine.
  globalThis.AudioContext = class {
    state = "running";
    createBuffer(_channels, count) { return { getChannelData: () => new Float32Array(count) }; }
    createBufferSource() { return { connect() {}, start() { queueMicrotask(() => this.onended?.()); } }; }
  };
  prefs.readAloudEngine = "builtin";
  const beforeBuiltin = lists;
  await speech.speakText("This is Patchbay.", "builtin");
  assert.equal(lists, beforeBuiltin);

  // A phone-shaped browser preview must keep using Web Speech, without native IPC.
  delete window.__TAURI_INTERNALS__;
  window.speechSynthesis.getVoices = () => [{ voiceURI: "browser", name: "Browser voice" }];
  assert.equal((await speech.systemVoices())[0].source, "web");
  console.log("speech: Android routing, native completion queue, Stop, visible errors, explicit Built-in and browser isolation passed");
} finally {
  await server.close();
}
