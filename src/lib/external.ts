import { openUrl } from "@tauri-apps/plugin-opener";
import { useStore } from "./store";
import { errorText } from "./ipc";
import { isWeb } from "./platform";

const EXTERNAL = /^(https?:|mailto:)/i;

export function isExternalUrl(href: string | undefined): href is string {
  return !!href && EXTERNAL.test(href);
}

/** Open a link in the system browser (the webview itself never navigates away). */
export function openExternal(href: string) {
  if (isWeb()) {
    window.open(href, "_blank", "noopener,noreferrer");
    return;
  }
  openUrl(href).catch((e) => useStore.getState().setToast(`Could not open link: ${errorText(e)}`));
}
