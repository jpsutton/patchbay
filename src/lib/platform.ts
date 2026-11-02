/**
 * A UUID v4. `crypto.randomUUID` is missing on plain `http://` LAN hosts (non-secure context),
 * which is how the Vite preview is opened from another computer — fall back to getRandomValues.
 */
export function uuid(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  b[6] = (b[6]! & 0x0f) | 0x40;
  b[8] = (b[8]! & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

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
