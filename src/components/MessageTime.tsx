import { useSyncExternalStore } from "react";
import { fmtAgo, fmtDateTime } from "../lib/format";

let now = Date.now();
const listeners = new Set<() => void>();
let timer: number | undefined;

function subscribe(listener: () => void) {
  listeners.add(listener);
  if (timer === undefined) {
    timer = window.setInterval(() => {
      now = Date.now();
      listeners.forEach((l) => l());
    }, 30_000);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && timer !== undefined) {
      window.clearInterval(timer);
      timer = undefined;
    }
  };
}

/** "12 minutes ago", with the exact time on a long press. One clock for every message. */
export function MessageTime({ at }: { at: number }) {
  const tick = useSyncExternalStore(subscribe, () => now, () => now);
  return (
    <time className="meta shrink-0 text-faint" dateTime={new Date(at).toISOString()} title={fmtDateTime(at)}>
      {fmtAgo(at, tick)}
    </time>
  );
}
