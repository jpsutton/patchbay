import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useStore } from "./store";
import { isWeb } from "./platform";

/** Whole-UI zoom, like a browser's: Ctrl/⌘ +, −, 0 and Ctrl/⌘ + scroll wheel. */
const KEY = "patchbay.zoom";
const LEVELS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];

let level = load();

function load(): number {
  try {
    const z = Number(localStorage.getItem(KEY));
    return LEVELS.includes(z) ? z : 1;
  } catch {
    return 1;
  }
}

function apply(z: number) {
  if (isWeb()) {
    // No webview zoom in a browser tab; CSS zoom scales the page the same way.
    document.documentElement.style.zoom = z === 1 ? "" : String(z);
    return;
  }
  getCurrentWebview()
    .setZoom(z)
    .catch((e) => console.warn("zoom:", e));
}

function set(z: number, announce = true) {
  level = z;
  apply(z);
  try {
    localStorage.setItem(KEY, String(z));
  } catch {
    /* storage unavailable */
  }
  if (announce) useStore.getState().setToast(`Zoom ${Math.round(z * 100)}%`);
}

function step(dir: 1 | -1) {
  const i = LEVELS.findIndex((l) => l >= level - 1e-6);
  const next = LEVELS[Math.min(LEVELS.length - 1, Math.max(0, (i < 0 ? LEVELS.indexOf(1) : i) + dir))];
  if (next !== level) set(next);
}

export const zoomIn = () => step(1);
export const zoomOut = () => step(-1);
export const zoomReset = () => set(1);

/** Restore the saved zoom and listen for Ctrl/⌘ + wheel. Call once at startup. */
let started = false;

export function initZoom(isMod: (e: { ctrlKey: boolean; metaKey: boolean }) => boolean) {
  // Once per page, even when React runs effects twice in development.
  if (started) return;
  started = true;
  if (level !== 1) set(level, false);
  let acc = 0;
  window.addEventListener(
    "wheel",
    (e) => {
      if (!isMod(e)) return;
      e.preventDefault();
      // Trackpads send many small deltas; step once per notch-sized amount.
      acc += e.deltaY;
      if (Math.abs(acc) < 50) return;
      if (acc < 0) zoomIn();
      else zoomOut();
      acc = 0;
    },
    { passive: false },
  );
}
