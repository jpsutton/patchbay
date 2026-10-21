import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { useStore } from "./store";
import { isRemoteApp, isWeb } from "./platform";

let allowed: boolean | null = null;

/** What a click on a notification opens. */
export type NotifyTarget = { kind: "chat" | "code"; id: string };

/** OS notification, only while Patchbay isn't the focused window and the preference is on. */
export async function notifyInBackground(title: string, body: string, open?: NotifyTarget) {
  // The phone app's backend raises these itself, also while the web view is paused.
  if (isRemoteApp()) return;
  if (!useStore.getState().prefs.notifications || document.hasFocus()) return;
  if (isWeb()) {
    if (typeof Notification === "undefined") return;
    if (Notification.permission === "default") await Notification.requestPermission();
    if (Notification.permission === "granted") {
      const n = new Notification(title, { body });
      n.onclick = () => {
        window.focus();
        if (open) openNotified(open);
        n.close();
      };
    }
    return;
  }
  try {
    if (allowed === null) {
      allowed = await isPermissionGranted();
      if (!allowed) allowed = (await requestPermission()) === "granted";
    }
    // The backend raises the window on a click and hands `open` back (see src-tauri/src/desktop.rs).
    if (allowed) sendNotification({ title, body, extra: open ? { open } : undefined });
  } catch {
    allowed = false;
  }
}

/** Show the conversation or Code session a clicked notification was about. */
export function openNotified(t: NotifyTarget) {
  const s = useStore.getState();
  s.setSettingsOpen(false);
  s.setMode(t.kind);
  if (t.kind === "code") s.selectCodeSession(t.id);
  else s.selectConversation(t.id);
}
