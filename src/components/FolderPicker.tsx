import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { api, errorText, type DirListing } from "../lib/ipc";
import { ChevronLeftIcon, ChevronRightIcon } from "./icons";

/** `~/…` for paths in the home folder. */
function tilde(path: string, home: string): string {
  return path === home || path.startsWith(home + "/") ? "~" + path.slice(home.length) : path;
}

function Picker({ title, done }: { title: string; done: (path: string | null) => void }) {
  const [listing, setListing] = useState<DirListing | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const open = (path: string | null) =>
    api.listDirs(path).then(
      (l) => {
        setListing(l);
        setErr(null);
      },
      (e) => setErr(errorText(e)),
    );

  useEffect(() => {
    open(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") done(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]" onClick={() => done(null)}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        className="flex max-h-[min(560px,85vh)] w-[min(520px,92vw)] flex-col rounded-2xl border border-border bg-raised shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 pb-2 pt-4 text-sm font-semibold">{title}</div>
        <div className="flex items-center gap-1 px-3 pb-2">
          <button
            className="flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg disabled:opacity-40"
            aria-label="Up one folder"
            disabled={!listing?.parent}
            onClick={() => listing?.parent && open(listing.parent)}
          >
            <ChevronLeftIcon size={16} />
          </button>
          <div className="min-w-0 flex-1 truncate font-mono text-xs text-muted">
            {listing ? tilde(listing.path, listing.home) : "…"}
          </div>
        </div>
        {err && <div className="px-5 pb-2 text-xs text-danger">{err}</div>}
        <div className="min-h-[160px] flex-1 overflow-y-auto border-y border-line py-1">
          {listing && listing.dirs.length === 0 && <div className="px-5 py-4 text-xs text-muted">No folders here.</div>}
          {listing?.dirs.map((d) => (
            <button
              key={d}
              className="flex w-full items-center gap-2 px-5 py-1.5 text-left text-sm hover:bg-panel-2"
              onClick={() => open(`${listing.path}/${d}`)}
            >
              <span className="min-w-0 flex-1 truncate">{d}</span>
              <ChevronRightIcon size={12} />
            </button>
          ))}
          {listing?.truncated && <div className="px-5 py-2 text-xs text-faint">Only the first 500 folders are shown.</div>}
        </div>
        <div className="flex justify-end gap-2 px-5 py-3">
          <button className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg" onClick={() => done(null)}>
            Cancel
          </button>
          <button
            disabled={!listing}
            className="h-[34px] max-w-[60%] truncate rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg disabled:opacity-50"
            onClick={() => listing && done(listing.path)}
          >
            Open {listing ? tilde(listing.path, listing.home) : ""}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Choose a folder on the computer running Patchbay, for a browser with no native dialog (the
 * daemon's web UI). Resolves null when cancelled.
 */
export function pickFolder(title: string): Promise<string | null> {
  return new Promise((resolve) => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = createRoot(host);
    const done = (path: string | null) => {
      root.unmount();
      host.remove();
      resolve(path);
    };
    root.render(<Picker title={title} done={done} />);
  });
}
