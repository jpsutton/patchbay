/**
 * What a session's sidebar row shows. A window knows a session's own state only after it opened,
 * started or attached to it. Until then its agent may still run, mid-turn, in patchbayd (or on the
 * app's own backend before a reload), so the row falls back to what the backend reports
 * (`code_live`). Kept free of the store so it runs under `node --experimental-strip-types`.
 */

/** The backend's report for one Code session (`Live` in crates/patchbay-mesh/src/remote.rs). */
export interface CodeLive {
  /** `starting`, `ready` or `exited`; empty when nothing is known. */
  status: string;
  busy: boolean;
  permissions: string[];
  questions?: string[];
  stalled?: boolean;
  last: number;
}

/** The part of a window's own state for a session that a row reads. */
export interface RowOwn {
  status: string;
  busy: boolean;
  permissions: readonly unknown[];
  questions: readonly unknown[];
  stalled: unknown | null;
  auth?: unknown | null;
}

export interface RowStatus {
  running: boolean;
  busy: boolean;
  attention: boolean;
  stalled: boolean;
}

/**
 * The row's state. `own` counts once the window did something with the session: status `idle` is
 * the empty state, which is also what stray live events leave behind for a session nobody
 * opened, so it does not count. A signed-out session stays neutral (no light, not busy).
 */
export function rowStatus(own: RowOwn | undefined, live: CodeLive | undefined): RowStatus {
  if (own && own.status !== "idle") {
    const signedOut = !!own.auth;
    return {
      running: !signedOut && (own.status === "ready" || own.status === "starting"),
      busy: !signedOut && own.busy,
      attention: own.permissions.length > 0 || own.questions.length > 0,
      stalled: !signedOut && own.busy && own.stalled != null,
    };
  }
  const running = live?.status === "ready" || live?.status === "starting";
  return {
    running,
    busy: running && !!live?.busy,
    attention: (live?.permissions.length ?? 0) > 0 || (live?.questions?.length ?? 0) > 0,
    stalled: running && !!live?.busy && !!live?.stalled,
  };
}

/** Whether any session the window has not opened waits on the user (the Code tab's light). */
export function liveNeedsYou(live: CodeLive | undefined): boolean {
  return (live?.permissions.length ?? 0) > 0 || (live?.questions?.length ?? 0) > 0;
}
