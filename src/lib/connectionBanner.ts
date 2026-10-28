import { listen } from "@tauri-apps/api/event";

/**
 * A red bar across the top while the backend is unreachable: patchbayd for a browser tab
 * (lib/webBackend.ts), or for the desktop app attached to it (src-tauri/src/daemon_link.rs).
 * Returns a function that removes it.
 */
export function showConnectionBanner(text: string): () => void {
  const bar = document.createElement("div");
  bar.textContent = text;
  bar.setAttribute("role", "status");
  Object.assign(bar.style, {
    position: "fixed",
    inset: "0 0 auto 0",
    zIndex: "9999",
    padding: "8px 12px",
    font: "13px system-ui, sans-serif",
    textAlign: "center",
    background: "#b42318",
    color: "#fff",
  });
  document.body.appendChild(bar);
  return () => bar.remove();
}

/**
 * Desktop app on patchbayd's backend: show the banner while the daemon is away. The app reloads
 * the window itself once it reconnects.
 */
export async function watchDaemonLink() {
  let hide: (() => void) | null = null;
  await listen("daemon://disconnected", () => {
    hide ??= showConnectionBanner("Lost the connection to patchbayd. Reconnecting…");
  });
  await listen("daemon://connected", () => {
    hide?.();
    hide = null;
  });
}
