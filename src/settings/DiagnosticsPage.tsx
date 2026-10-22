import { useEffect, useRef, useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  AGENT_LABELS,
  api,
  errorText,
  type WireFrame,
  type WireLogStatus,
} from "../lib/ipc";
import { isWeb } from "../lib/platform";
import { useStore } from "../lib/store";
import { Row, Toggle } from "./PreferencesPage";

/** The newest frames the viewer lists; saving to a file keeps them all. */
const SHOWN = 500;

/** The ACP wire log: raw messages between Patchbay and the coding agents on this computer. */
export function DiagnosticsPage() {
  const setToast = useStore((s) => s.setToast);
  const [status, setStatus] = useState<WireLogStatus | null>(null);
  const [frames, setFrames] = useState<WireFrame[]>([]);
  const seen = useRef("");

  // Polled while open; the frames are fetched again only when the log changed.
  const reload = async () => {
    const st = await api.wireLogStatus();
    setStatus(st);
    const key = `${st.frames}/${st.dropped}`;
    if (key !== seen.current) {
      seen.current = key;
      setFrames(await api.wireLogFrames(undefined, SHOWN));
    }
  };
  useEffect(() => {
    reload().catch((e) => setToast(errorText(e)));
    const t = setInterval(() => reload().catch(() => {}), 2000);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = (f: () => Promise<unknown>) => async () => {
    try {
      await f();
      await reload();
    } catch (e) {
      setToast(errorText(e));
    }
  };
  const save = run(async () => {
    const path = await api.wireLogSave();
    setToast(`Saved to ${path}`);
    if (!isWeb()) await revealItemInDir(path);
  });

  if (!status) return null;
  const minutes = Math.ceil(status.remainingSecs / 60);
  return (
    <div className="max-w-2xl">
      <Row
        label="Record the ACP wire log"
        hint={
          <>
            Raw messages between Patchbay and the coding agents running on this
            computer, for reporting agent problems. API keys, tokens and
            environment values are removed, and very large messages are cut.
            Kept in memory only, and turns itself off after 30 minutes.
            {status.on &&
              ` Recording; ${minutes} minute${minutes === 1 ? "" : "s"} left.`}
          </>
        }
      >
        <Toggle
          label="Record the ACP wire log"
          checked={status.on}
          onChange={(on) => run(() => api.wireLogSet(on))()}
        />
      </Row>
      <Row
        label={`${status.frames} message${status.frames === 1 ? "" : "s"} recorded`}
        hint={
          status.dropped > 0
            ? `The oldest ${status.dropped} were dropped to keep the log small.`
            : undefined
        }
      >
        <div className="flex items-center gap-2">
          <button
            className="rounded-md border border-border bg-panel px-3 py-1.5 text-xs hover:bg-panel-2 disabled:opacity-40"
            disabled={status.frames === 0}
            onClick={save}
          >
            Save to file
          </button>
          <button
            className="text-xs text-muted hover:text-danger disabled:opacity-40"
            disabled={status.frames === 0}
            onClick={run(api.wireLogClear)}
          >
            Clear
          </button>
        </div>
      </Row>
      {frames.length > 0 && (
        <div className="mb-4 mt-2 flex flex-col divide-y divide-line rounded-lg border border-line font-mono text-[11.5px]">
          {frames.map((f) => (
            <details key={f.seq} className="px-2 py-1">
              <summary className="flex cursor-pointer list-none gap-2 text-muted">
                <span className="shrink-0 text-faint">
                  {new Date(f.at).toLocaleTimeString()}
                </span>
                <span className="shrink-0">
                  {(AGENT_LABELS as Record<string, string>)[f.agent] ?? f.agent}
                </span>
                <span
                  className={`shrink-0 ${f.dir === "in" ? "text-accent-ink" : ""}`}
                  title={f.dir === "in" ? "From the agent" : "To the agent"}
                >
                  {f.dir === "in" ? "←" : "→"}
                </span>
                <span className="min-w-0 flex-1 truncate text-fg/85">
                  {f.text}
                </span>
              </summary>
              <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap break-all text-fg/85">
                {f.text}
              </pre>
            </details>
          ))}
        </div>
      )}
    </div>
  );
}
