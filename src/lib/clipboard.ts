import { readText as nativeRead, writeText as nativeWrite } from "@tauri-apps/plugin-clipboard-manager";
import { isWeb } from "./platform";

// The app's webviews can't be relied on to read the clipboard (WebKitGTK denies it), so the
// native app goes through the clipboard plugin. A browser served by patchbayd, or the dev fake
// backend, uses the browser's own clipboard.
const native = () => "__TAURI_INTERNALS__" in window && !isWeb();

export async function copyText(text: string): Promise<void> {
  if (native()) {
    try {
      return await nativeWrite(text);
    } catch {
      /* fall back to the browser's clipboard */
    }
  }
  await navigator.clipboard.writeText(text);
}

export async function pasteText(): Promise<string> {
  if (native()) {
    try {
      return (await nativeRead()) ?? "";
    } catch {
      /* fall back to the browser's clipboard */
    }
  }
  return navigator.clipboard.readText();
}
