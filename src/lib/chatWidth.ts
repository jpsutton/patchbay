import { useSyncExternalStore } from "react";

/**
 * Width of the conversation column in Chat and Code, set from the header's width control.
 * The minimum is the original fixed width. It is a per-device setting (screen sizes differ),
 * so it lives in localStorage, like zoom and the sidebar width.
 */
const KEY = "patchbay.chatWidth";
export const CHAT_WIDTH_MIN = 768;
export const CHAT_WIDTH_MAX = 1920;
export const CHAT_WIDTH_STEP = 32;

const clamp = (w: number) => Math.round(Math.min(CHAT_WIDTH_MAX, Math.max(CHAT_WIDTH_MIN, w)));

function load(): number {
  try {
    const w = Number(localStorage.getItem(KEY));
    return w ? clamp(w) : CHAT_WIDTH_MIN;
  } catch {
    return CHAT_WIDTH_MIN;
  }
}

let width = load();
const listeners = new Set<() => void>();

function apply() {
  // The stylesheet default (`:root { --chat-width }`) is the minimum, so only override above it.
  if (width === CHAT_WIDTH_MIN) document.documentElement.style.removeProperty("--chat-width");
  else document.documentElement.style.setProperty("--chat-width", `${width}px`);
}

// The phone app has no control for this and never sets it, so it keeps the stylesheet default.
apply();

export function setChatWidth(w: number) {
  const next = clamp(w);
  if (next === width) return;
  width = next;
  apply();
  try {
    if (next === CHAT_WIDTH_MIN) localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, String(next));
  } catch {
    /* storage unavailable */
  }
  listeners.forEach((l) => l());
}

export const resetChatWidth = () => setChatWidth(CHAT_WIDTH_MIN);

export function useChatWidth(): number {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => width,
  );
}
