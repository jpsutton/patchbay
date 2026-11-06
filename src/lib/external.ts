import { openUrl } from "@tauri-apps/plugin-opener";
import { useStore } from "./store";
import { errorText } from "./ipc";
import { isRemoteApp, isWeb } from "./platform";
import { parseShareUrl } from "./shareUrl";

const EXTERNAL = /^(https?:|mailto:)/i;

export function isExternalUrl(href: string | undefined): href is string {
  return !!href && EXTERNAL.test(href);
}

/**
 * Open a link. On the phone, an artifact share link opens in the in-app embedded viewer instead
 * of leaving the app; `raw` bypasses that interception (the viewer's "open in browser" fallback).
 * Otherwise it goes to the system browser (or a new tab in the daemon's browser UI).
 */
export function openExternal(href: string, opts?: { raw?: boolean }) {
  if (!opts?.raw && isRemoteApp() && parseShareUrl(href)) {
    useStore.getState().setShareUrl(href);
    return;
  }
  if (isWeb()) {
    window.open(href, "_blank", "noopener,noreferrer");
    return;
  }
  openUrl(href).catch((e) => useStore.getState().setToast(`Could not open link: ${errorText(e)}`));
}
