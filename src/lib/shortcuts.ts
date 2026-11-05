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

/**
 * The Code pane a view sits in. Null outside a pane host (the phone app), where the open session
 * is the store's. `focused`: this pane has focus. `split`: several panes are on screen, so input
 * meant for "the view" (file drops, shortcuts) has to find its own pane. `last`: the rightmost pane.
 * `closable`: the session is one of two in a split, even when the window is too narrow to show both.
 */
export interface PaneInfo {
  id: string | null;
  focused: boolean;
  split: boolean;
  last: boolean;
  closable: boolean;
}
export const PaneContext = createContext<PaneInfo | null>(null);

/** Subscribe the visible view to global shortcut actions. */
export function useViewAction(handler: (a: ViewAction) => void) {
  const active = useContext(ViewActive);
  const focused = useContext(PaneContext)?.focused ?? true;
  const ref = useRef(handler);
  ref.current = handler;
  const activeRef = useRef(active && focused);
  activeRef.current = active && focused;
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
  return isMac
    ? keys.replace(/Ctrl\+/g, "⌃").replace(/Alt\+/g, "⌥").replace(/Mod\+/g, "⌘").replace(/Shift\+/g, "⇧")
    : keys.replace(/Mod\+/g, "Ctrl+");
}

/**
 * The split-pane keys: Ctrl+] and Ctrl+[ move focus, ⌘Ctrl+W (Ctrl+Alt+W elsewhere) closes the focused
 * pane. Ctrl+[ is also Esc to a terminal, so a terminal lets these through only while split.
 */
export function paneShortcut(e: KeyboardEvent): "next" | "previous" | "close" | null {
  if (e.shiftKey) return null;
  if (e.ctrlKey && !e.altKey && !e.metaKey) return e.key === "]" ? "next" : e.key === "[" ? "previous" : null;
  const chord = isMac ? e.metaKey && e.ctrlKey && !e.altKey : e.ctrlKey && e.altKey && !e.metaKey;
  return chord && e.code === "KeyW" ? "close" : null;
}

export function focusComposer() {
  requestAnimationFrame(() => {
    const root = '[data-view-active="true"]';
    // In a split, the focused pane's composer; otherwise the view's only one.
    (
      document.querySelector<HTMLTextAreaElement>(`${root} [data-pane-focused="true"] textarea[data-composer]`) ??
      document.querySelector<HTMLTextAreaElement>(`${root} textarea[data-composer]`)
    )?.focus();
  });
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
  { keys: "Mod+Shift+N", label: "New window" },
  { keys: "Mod+K", label: "Search everything and run commands" },
  { keys: "Mod+Shift+F", label: "Filter the sidebar" },
  { keys: "Mod+B", label: "Show or hide the sidebar" },
  { keys: isMac ? "Mod+[" : "Alt+←", label: "Back to the previous chat or session" },
  { keys: isMac ? "Mod+]" : "Alt+→", label: "Forward" },
  { keys: "Mod+,", label: "Open settings" },
  { keys: "Mod+= / Mod+-", label: "Zoom in / out (or Mod+scroll)" },
  { keys: "Mod+0", label: "Reset zoom" },
  { keys: "Mod+Shift+C", label: "Copy the last reply" },
  { keys: isMac ? "Option-click" : "Alt+click", label: "Open a Code session beside this one (split view)" },
  { keys: "Ctrl+] / Ctrl+[", label: "Next / previous split pane" },
  { keys: isMac ? "Ctrl+Mod+W" : "Ctrl+Alt+W", label: "Close the focused split pane" },
  { keys: "Esc", label: "Stop the reply or agent turn" },
  { keys: "↑", label: "Edit your last message (in an empty composer)" },
  { keys: "Mod+/", label: "Show keyboard shortcuts" },
];
