/** Run: node src/lib/speech.selftest.mjs */
import assert from "node:assert/strict";
import { createServer } from "vite";

let lists = 0;
let speaks = [];
let stops = 0;
let listError = null;
let complete;
const streams = [];
const cancelled = [];
const prefs = { readAloudEngine: "auto", readAloudVoice: "pixel", readAloudRate: "normal" };
globalThis.__speechTest = {
  prefs,
  api: {
    listNativeVoices: async () => { lists++; if (listError) throw listError; return [{ id: "pixel", name: "Pixel voice" }]; },
    speakNative: (text, voice, rate) => { speaks.push({ text, voice, rate }); return new Promise((r) => { complete = r; }); },
    stopNative: async () => { stops++; complete?.(); },
    synthesizeSpeech: (input, onAudio) => { streams.push({ input, onAudio }); return new Promise((r) => { streams.at(-1).done = r; }); },
    cancelSpeech: async (id) => { cancelled.push(id); streams.find((s) => s.input.streamId === id)?.done(); },
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
  const sources = [];
  let autoEnd = true;
  globalThis.AudioContext = class {
    state = "running";
    currentTime = 0;
    destination = {};
    createBuffer(_channels, count, rate) { return { duration: count / rate, getChannelData: () => new Float32Array(count) }; }
    createBufferSource() {
      const src = {
        connect() {},
        start(at) { src.at = at; sources.push(src); if (autoEnd) queueMicrotask(() => src.onended?.()); },
        stop() { src.stopped = true; src.onended?.(); },
      };
      return src;
    }
  };
  prefs.readAloudEngine = "builtin";
  const beforeBuiltin = lists;
  await speech.speakText("This is Patchbay.", "builtin");
  assert.equal(lists, beforeBuiltin);

  // A speech server streams raw PCM that plays while it arrives, and Stop drops the request.
  autoEnd = false;
  sources.length = 0;
  prefs.readAloudEngine = "server";
  delete window.__PATCHBAY_REMOTE__;
  const pcm = (samples) => Buffer.from(new Int16Array(samples).buffer).toString("base64");
  const served = speech.speakText("Stream this.", "server");
  await turn();
  assert.equal(streams.length, 1, "one request per piece");
  streams[0].onAudio({ type: "start", mime: "audio/pcm", pcm: true });
  streams[0].onAudio({ type: "chunk", data: pcm(2400) });
  await turn();
  assert.equal(sources.length, 1, "audio plays before the request ends");
  streams[0].onAudio({ type: "chunk", data: pcm(2400) });
  await turn();
  assert.ok(sources[1].at >= sources[0].at + 0.1 - 1e-9, "pieces play back to back");
  speech.stopSpeech(); await served;
  assert.deepEqual(cancelled, [streams[0].input.streamId], "Stop drops the request");
  assert.ok(sources.every((src) => src.stopped), "Stop silences what is scheduled");
  assert.equal(speech.speakingNow(), null);
  autoEnd = true;
  window.__PATCHBAY_REMOTE__ = true;
  prefs.readAloudEngine = "builtin";

  // A phone-shaped browser preview must keep using Web Speech, without native IPC.
  delete window.__TAURI_INTERNALS__;
  window.speechSynthesis.getVoices = () => [{ voiceURI: "browser", name: "Browser voice" }];
  assert.equal((await speech.systemVoices())[0].source, "web");
  console.log("speech: Android routing, native completion queue, Stop, visible errors, explicit Built-in, streamed server audio and browser isolation passed");
} finally {
  await server.close();
}
