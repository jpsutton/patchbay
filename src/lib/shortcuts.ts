import { createContext, useContext, useEffect, useRef } from "react";

/** Actions the global shortcut handler asks the active view to perform. */
export type ViewAction = "stop" | "copy-last";

const EVENT = "patchbay:action";

export function emitViewAction(a: ViewAction) {
  window.dispatchEvent(new CustomEvent<ViewAction>(EVENT, { detail: a }));
}

/**
 * Whether the enclosing view (Chat or Code) is the one on screen. Both stay mounted so switching
 * tabs is instant, so window-wide input (shortcuts, file drops) must go to the visible one only.
 */
export const ViewActive = createContext(true);

/** Subscribe the visible view to global shortcut actions. */
export function useViewAction(handler: (a: ViewAction) => void) {
  const active = useContext(ViewActive);
  const ref = useRef(handler);
  ref.current = handler;
  const activeRef = useRef(active);
  activeRef.current = active;
  useEffect(() => {
    const on = (e: Event) => {
      if (activeRef.current) ref.current((e as CustomEvent<ViewAction>).detail);
    };
    window.addEventListener(EVENT, on);
    return () => window.removeEventListener(EVENT, on);
  }, []);
}

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/** Ctrl on Linux/Windows, ⌘ on macOS. */
export function isMod(e: { ctrlKey: boolean; metaKey: boolean }): boolean {
  return isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
}

export function keyLabel(keys: string): string {
  return isMac ? keys.replace(/Mod\+/g, "⌘").replace(/Shift\+/g, "⇧") : keys.replace(/Mod\+/g, "Ctrl+");
}

export function focusComposer() {
  requestAnimationFrame(() =>
    document.querySelector<HTMLTextAreaElement>('[data-view-active="true"] textarea[data-composer]')?.focus(),
  );
}

export function focusSidebarSearch() {
  requestAnimationFrame(() => {
    const el = document.getElementById("sidebar-search") as HTMLInputElement | null;
    el?.focus();
    el?.select();
  });
}

export const SHORTCUTS: { keys: string; label: string }[] = [
  { keys: "Mod+N", label: "New chat (Code: new session in this folder)" },
  { keys: "Mod+K", label: "Search everything and run commands" },
  { keys: "Mod+Shift+F", label: "Filter the sidebar" },
  { keys: "Mod+B", label: "Show or hide the sidebar" },
  { keys: isMac ? "Mod+[" : "Alt+←", label: "Back to the previous chat or session" },
  { keys: isMac ? "Mod+]" : "Alt+→", label: "Forward" },
  { keys: "Mod+,", label: "Open settings" },
  { keys: "Mod+= / Mod+-", label: "Zoom in / out (or Mod+scroll)" },
  { keys: "Mod+0", label: "Reset zoom" },
  { keys: "Mod+Shift+C", label: "Copy the last reply" },
  { keys: "Esc", label: "Stop the reply or agent turn" },
  { keys: "↑", label: "Edit your last message (in an empty composer)" },
  { keys: "Mod+/", label: "Show keyboard shortcuts" },
];
