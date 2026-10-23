/**
 * One Read aloud queue for every surface.
 *
 * Automatic picks a voice that exists here: the Mac's `say`, the browser or Android
 * `speechSynthesis` when it lists voices, and the built-in voice on Linux (Ubuntu's
 * WebKit build has speech synthesis turned off) and anywhere else with no system voice.
 * A speech server is explicit. Its clip is fetched where the API key lives and played
 * on this screen.
 */

import { api, type Preferences } from "./ipc";
import { isDesktopApp, isRemoteApp } from "./platform";
import { isMac } from "./shortcuts";
import { useStore } from "./store";
import { SAMPLE_RATE, renderSpeech, type SpeechRate } from "./builtinVoice";
import { speechChunks, spokenText } from "./spokenText";

export type SpeechEngine = "system" | "builtin" | "server";

export interface SpeechVoice {
  id: string;
  name: string;
  source: "web" | "native";
}

type Listener = () => void;

let generation = 0;
let speakingId: string | null = null;
const listeners = new Set<Listener>();
let audioCtx: AudioContext | null = null;
let htmlAudio: HTMLAudioElement | null = null;
let audioUrl: string | null = null;
let nativeVoices: SpeechVoice[] | null = null;

export function speakingNow(): string | null {
  return speakingId;
}

export function subscribeSpeaking(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function setSpeaking(id: string | null) {
  speakingId = id;
  listeners.forEach((l) => l());
}

function isLinuxDesktop(): boolean {
  return isDesktopApp() && !isMac && /Linux/i.test(navigator.userAgent);
}

/** Call from a click so later playback is allowed to make sound. */
export function primeAudio() {
  const ctx = context();
  if (ctx.state === "suspended") void ctx.resume();
}

function context(): AudioContext {
  if (!audioCtx) audioCtx = new AudioContext();
  return audioCtx;
}

export async function systemVoices(): Promise<SpeechVoice[]> {
  // The Mac app speaks with `say`. Its WebKit voice list is a different engine.
  if (isDesktopApp() && isMac) {
    const native = await loadNativeVoices();
    if (native.length > 0) return native;
  }
  // Ubuntu's WebKitGTK is built with speech synthesis disabled, so that list is not a voice.
  if (isLinuxDesktop()) return loadNativeVoices();
  const web = await webVoices();
  if (web.length > 0) return web;
  return isDesktopApp() ? loadNativeVoices() : web;
}

function webVoices(): Promise<SpeechVoice[]> {
  const synth = window.speechSynthesis;
  if (!synth) return Promise.resolve([]);
  const map = () =>
    synth.getVoices().map((v) => ({ id: v.voiceURI || v.name, name: v.name, source: "web" as const }));
  const ready = map();
  if (ready.length > 0) return Promise.resolve(ready);
  return new Promise((resolve) => {
    const done = () => resolve(map());
    synth.addEventListener("voiceschanged", done, { once: true });
    window.setTimeout(done, 400);
  });
}

async function loadNativeVoices(): Promise<SpeechVoice[]> {
  if (nativeVoices) return nativeVoices;
  if (!isDesktopApp()) return [];
  try {
    const list = await api.listNativeVoices();
    nativeVoices = list.map((v) => ({ id: v.id, name: v.name, source: "native" as const }));
  } catch {
    nativeVoices = [];
  }
  return nativeVoices;
}

export function resolvedEngine(prefs: Preferences, voices: SpeechVoice[]): SpeechEngine {
  if (prefs.readAloudEngine === "server") return "server";
  if (prefs.readAloudEngine === "builtin") return "builtin";
  // An explicit system choice stays system, and fails out loud when this device has no voice.
  if (prefs.readAloudEngine === "system") return "system";
  if (voices.some((v) => v.source === "native") && isDesktopApp() && isMac) return "system";
  if (isLinuxDesktop()) return "builtin";
  if (voices.length > 0) return "system";
  return "builtin";
}

export function stopSpeech() {
  generation += 1;
  setSpeaking(null);
  window.speechSynthesis?.cancel();
  if (htmlAudio) {
    htmlAudio.pause();
    htmlAudio.src = "";
    htmlAudio = null;
  }
  if (audioUrl) {
    URL.revokeObjectURL(audioUrl);
    audioUrl = null;
  }
  if (isDesktopApp()) void api.stopNative().catch(() => {});
}

/** Speak a reply. A second call replaces whatever is playing. */
export async function speakText(markdown: string, id: string): Promise<void> {
  const text = spokenText(markdown);
  if (!text) return;
  stopSpeech();
  const token = generation;
  setSpeaking(id);
  const prefs = useStore.getState().prefs;
  try {
    const voices = await systemVoices();
    if (token !== generation) return;
    const engine = resolvedEngine(prefs, voices);
    if (engine === "server") await speakServer(text, prefs, token);
    else if (engine === "builtin") await speakBuiltin(text, prefs.readAloudRate, token);
    else await speakSystem(text, prefs, voices, token);
  } catch (e) {
    if (token === generation) {
      setSpeaking(null);
      throw e;
    }
    return;
  }
  if (token === generation) setSpeaking(null);
}

async function speakBuiltin(text: string, rate: SpeechRate, token: number) {
  primeAudio();
  for (const chunk of speechChunks(text)) {
    if (token !== generation) return;
    const samples = renderSpeech(chunk, rate);
    if (samples.length === 0) continue;
    await playSamples(samples, token);
  }
}

function playSamples(samples: Float32Array, token: number): Promise<void> {
  const ctx = context();
  const buffer = ctx.createBuffer(1, samples.length, SAMPLE_RATE);
  buffer.getChannelData(0).set(samples);
  const src = ctx.createBufferSource();
  src.buffer = buffer;
  src.connect(ctx.destination);
  return new Promise((resolve) => {
    const finish = () => resolve();
    src.onended = finish;
    src.start();
    const timer = window.setInterval(() => {
      if (token !== generation) {
        window.clearInterval(timer);
        try {
          src.stop();
        } catch {
          /* already ended */
        }
        finish();
      }
    }, 40);
    src.onended = () => {
      window.clearInterval(timer);
      finish();
    };
  });
}

async function speakSystem(text: string, prefs: Preferences, voices: SpeechVoice[], token: number) {
  const native = voices.some((v) => v.source === "native");
  if (native && isDesktopApp()) {
    for (const chunk of speechChunks(text)) {
      if (token !== generation) return;
      await api.speakNative(chunk, prefs.readAloudVoice, prefs.readAloudRate);
    }
    return;
  }
  if (voices.some((v) => v.source === "web") || webHasVoicesNow()) {
    for (const chunk of speechChunks(text)) {
      if (token !== generation) return;
      await speakWeb(chunk, prefs);
    }
    return;
  }
  if (prefs.readAloudEngine === "system") {
    throw new Error("No system voice on this computer. Choose Built-in in Settings.");
  }
  await speakBuiltin(text, prefs.readAloudRate, token);
}

function webHasVoicesNow(): boolean {
  return (window.speechSynthesis?.getVoices().length ?? 0) > 0;
}

function speakWeb(text: string, prefs: Preferences): Promise<void> {
  const synth = window.speechSynthesis;
  if (!synth) return Promise.reject(new Error("this browser has no system voice"));
  return new Promise((resolve, reject) => {
    const utter = new SpeechSynthesisUtterance(text);
    const voice = synth.getVoices().find((v) => v.voiceURI === prefs.readAloudVoice || v.name === prefs.readAloudVoice);
    if (voice) utter.voice = voice;
    utter.rate = prefs.readAloudRate === "slower" ? 0.85 : prefs.readAloudRate === "faster" ? 1.15 : 1;
    utter.onend = () => resolve();
    utter.onerror = () => reject(new Error("the system voice stopped"));
    synth.speak(utter);
  });
}

async function speakServer(text: string, prefs: Preferences, token: number) {
  if (isRemoteApp()) throw new Error("the phone speaks with its own voice. A speech server is configured on a computer.");
  for (const chunk of speechChunks(text, 1500)) {
    if (token !== generation) return;
    const clip = await api.synthesizeSpeech({
      providerId: prefs.readAloudServerProviderId,
      baseUrl: prefs.readAloudServerUrl,
      model: prefs.readAloudServerModel,
      voice: prefs.readAloudServerVoice,
      text: chunk,
    });
    if (!clip?.audioBase64) throw new Error("the speech server returned no audio");
    if (token !== generation) return;
    await playClip(clip.audioBase64, clip.mime || "audio/mpeg", token);
  }
}

function playClip(audioBase64: string, mime: string, token: number): Promise<void> {
  const bytes = Uint8Array.from(atob(audioBase64), (c) => c.charCodeAt(0));
  const url = URL.createObjectURL(new Blob([bytes], { type: mime }));
  audioUrl = url;
  const audio = new Audio(url);
  htmlAudio = audio;
  return new Promise((resolve, reject) => {
    const finish = () => {
      URL.revokeObjectURL(url);
      if (audioUrl === url) audioUrl = null;
      if (htmlAudio === audio) htmlAudio = null;
      resolve();
    };
    audio.onended = finish;
    audio.onerror = () => reject(new Error("could not play the speech server's audio"));
    void audio.play().catch((e) => reject(e instanceof Error ? e : new Error("could not play audio")));
    const timer = window.setInterval(() => {
      if (token !== generation) {
        window.clearInterval(timer);
        audio.pause();
        finish();
      }
    }, 40);
    audio.onended = () => {
      window.clearInterval(timer);
      finish();
    };
  });
}
