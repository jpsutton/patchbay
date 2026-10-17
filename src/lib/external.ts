import { openUrl } from "@tauri-apps/plugin-opener";
import { useStore } from "./store";
import { errorText } from "./ipc";

const EXTERNAL = /^(https?:|mailto:)/i;

export function isExternalUrl(href: string | undefined): href is string {
  return !!href && EXTERNAL.test(href);
}

/** Open a link in the system browser (the webview itself never navigates away). */
export function openExternal(href: string) {
  openUrl(href).catch((e) => useStore.getState().setToast(`Could not open link: ${errorText(e)}`));
}
