import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { useStore } from "./store";

let allowed: boolean | null = null;

/** OS notification, only while Patchbay isn't the focused window and the preference is on. */
export async function notifyInBackground(title: string, body: string) {
  if (!useStore.getState().prefs.notifications || document.hasFocus()) return;
  try {
    if (allowed === null) {
      allowed = await isPermissionGranted();
      if (!allowed) allowed = (await requestPermission()) === "granted";
    }
    if (allowed) sendNotification({ title, body });
  } catch {
    allowed = false;
  }
}
