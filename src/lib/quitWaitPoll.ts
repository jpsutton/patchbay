/**
 * When the wait dialog may call `quit_wait_status` (pb-20261111-66).
 *
 * Only the native desktop shell polls, and only while the dialog is showing:
 *
 * - a patchbayd web UI window would pay one failed round-trip per tick (the daemon answers
 *   "unknown command quit_wait_status", 4x/s per polling window) — it must make ZERO calls;
 * - a closed dialog (Keep Patchbay open, a finished quit, a cancelled wait) must not poll
 *   forever — the interval only exists while `showing` is true.
 *
 * The reload one-shot in QuitWaitDialog is gated by `desktop` alone (it is one call per
 * mount, not a poll) and never runs on the web UI either.
 */
export function shouldPollQuitWait(opts: { desktop: boolean; showing: boolean }): boolean {
  return opts.desktop && opts.showing;
}
