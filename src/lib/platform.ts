/**
 * True inside Patchbay Remote, the phone app (remote.html sets the flag before any module runs).
 * Its backend answers the desktop's commands through a gateway computer, so views hide what a
 * phone can't do: hosting work, terminals, moving sessions, saving files.
 */
export function isRemoteApp(): boolean {
  return typeof window !== "undefined" && (window as { __PATCHBAY_REMOTE__?: boolean }).__PATCHBAY_REMOTE__ === true;
}

/**
 * True when patchbayd serves this UI to a browser (lib/webBackend.ts). Commands reach the daemon
 * as usual, but native dialogs, the file manager, window controls and drops of local paths don't
 * exist, so views use the browser's equivalents.
 */
export function isWeb(): boolean {
  return typeof window !== "undefined" && (window as { __PATCHBAY_WEB__?: boolean }).__PATCHBAY_WEB__ === true;
}

/** The desktop app, as opposed to a browser on patchbayd, the phone, or `pnpm dev`. */
export function isDesktopApp(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window && !isWeb() && !isRemoteApp();
}

/** The installed Android phone app; browser previews keep using Web Speech. */
export function isAndroidApp(): boolean {
  return isRemoteApp() && "__TAURI_INTERNALS__" in window && /Android/i.test(navigator.userAgent);
}
