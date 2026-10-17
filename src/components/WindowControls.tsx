import { useEffect, useState, type ReactNode } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useStore } from "../lib/store";
import { CloseIcon, MaximizeIcon, MinimizeIcon, RestoreIcon } from "./icons";

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** True when the window manager's title bar is off and Patchbay draws its own controls. */
export function useAppTitleBar(): boolean {
  return useStore((s) => s.prefs.titleBar === "app");
}

/**
 * Props that make a bar drag the window when the title bar is app-drawn. Tauri only drags from
 * the element carrying the attribute itself, so give it to the bar and its plain-text children.
 */
export function useDragRegion(): { "data-tauri-drag-region"?: true } {
  return useAppTitleBar() ? { "data-tauri-drag-region": true } : {};
}

function useMaximized(): boolean {
  const [max, setMax] = useState(false);
  useEffect(() => {
    if (!inTauri) return;
    const w = getCurrentWindow();
    let un: (() => void) | undefined;
    const sync = () => w.isMaximized().then(setMax).catch(() => {});
    sync();
    w.onResized(sync).then((u) => (un = u));
    return () => un?.();
  }, []);
  return max;
}

/**
 * Rounds the window's corners when Patchbay draws its own title bar on Linux, where the main
 * window is transparent (src-tauri/tauri.linux.conf.json) so the page can draw its own edge.
 * Square again when maximized or fullscreen. Windows 11 rounds undecorated windows itself.
 */
export function useRoundedWindow() {
  const on = useAppTitleBar() && inTauri && navigator.userAgent.includes("Linux");
  const [filled, setFilled] = useState(false);
  useEffect(() => {
    if (!on) return;
    const w = getCurrentWindow();
    let un: (() => void) | undefined;
    const sync = () =>
      Promise.all([w.isMaximized(), w.isFullscreen()])
        .then(([max, full]) => setFilled(max || full))
        .catch(() => {});
    sync();
    w.onResized(sync).then((u) => (un = u));
    return () => un?.();
  }, [on]);
  const rounded = on && !filled;
  useEffect(() => {
    document.documentElement.classList.toggle("rounded-window", rounded);
  }, [rounded]);
}

function ControlButton({
  label,
  danger,
  onClick,
  children,
}: {
  label: string;
  danger?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      className={`flex h-8 w-9 items-center justify-center rounded-md text-fg/75 ${
        danger ? "hover:bg-danger hover:text-white" : "hover:bg-panel-2 hover:text-fg"
      }`}
      onClick={onClick}
      title={label}
      aria-label={label}
    >
      {children}
    </button>
  );
}

/** Minimize, maximize/restore and close, shown only with the app-drawn title bar. */
export function WindowControls() {
  const on = useAppTitleBar();
  const max = useMaximized();
  if (!on) return null;
  const w = () => getCurrentWindow();
  return (
    <div className="ml-2 flex shrink-0 items-center gap-0.5 border-l border-border pl-2">
      <ControlButton label="Minimize" onClick={() => inTauri && w().minimize()}>
        <MinimizeIcon />
      </ControlButton>
      <ControlButton label={max ? "Restore" : "Maximize"} onClick={() => inTauri && w().toggleMaximize()}>
        {max ? <RestoreIcon /> : <MaximizeIcon />}
      </ControlButton>
      <ControlButton label="Close" danger onClick={() => inTauri && w().close()}>
        <CloseIcon />
      </ControlButton>
    </div>
  );
}
