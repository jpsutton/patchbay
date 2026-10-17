/**
 * True inside Patchbay Remote, the phone app (remote.html sets the flag before any module runs).
 * Its backend answers the desktop's commands through a gateway computer, so views hide what a
 * phone can't do: hosting work, terminals, moving sessions, saving files.
 */
export function isRemoteApp(): boolean {
  return typeof window !== "undefined" && (window as { __PATCHBAY_REMOTE__?: boolean }).__PATCHBAY_REMOTE__ === true;
}
