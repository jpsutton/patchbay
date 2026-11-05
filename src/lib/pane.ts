import { useContext } from "react";
import { PaneContext } from "./shortcuts";
import { useStore } from "./store";

/** The Code session this view shows: its pane's, or the open one outside a pane host. */
export function usePaneSession(): string | null {
  const pane = useContext(PaneContext);
  const open = useStore((s) => s.activeCodeSessionId);
  return pane ? pane.id : open;
}
