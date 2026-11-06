import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { api, errorText, type ShareFetch } from "../lib/ipc";
import { useStore } from "../lib/store";
import { openExternal } from "../lib/external";
import { parseShareUrl } from "../lib/shareUrl";
import { ArrowRightIcon, FileTextIcon, GlobeIcon, RestartIcon, TriangleAlertIcon, XIcon } from "./icons";

/** Exact response header from the owning node (relayed through the gateway). */
function headerOf(f: ShareFetch, name: string): string | undefined {
  return f.headers.find(([k]) => k.toLowerCase() === name)?.[1];
}

/** filename from Content-Disposition, for the viewer's title. */
function filenameOf(f: ShareFetch): string | null {
  const cd = headerOf(f, "content-disposition");
  const m = cd && /filename="([^"]*)"/.exec(cd);
  return m?.[1] ?? null;
}

/** UTF-8 text from the base64 artifact bytes (HTML). */
function textOf(b64: string): string {
  try {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return "";
  }
}

/** A clear, non-blank state with an "open in browser" fallback and optional retry. */
function Message({
  title,
  body,
  onRetry,
  onBrowser,
}: {
  title: string;
  body: string;
  onRetry?: () => void;
  onBrowser: () => void;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
      <TriangleAlertIcon size={28} className="text-danger" />
      <div className="text-[15px] font-medium text-fg">{title}</div>
      <div className="max-w-sm text-[13px] leading-5 text-muted">{body}</div>
      <div className="mt-1 flex flex-wrap items-center justify-center gap-2">
        {onRetry && (
          <button
            type="button"
            className="inline-flex h-9 items-center gap-1.5 rounded-lg border border-border bg-panel px-3 text-[13px] text-fg hover:bg-panel-2"
            onClick={onRetry}
          >
            <RestartIcon size={14} /> Retry
          </button>
        )}
        <button
          type="button"
          className="inline-flex h-9 items-center gap-1.5 rounded-lg border border-border bg-panel px-3 text-[13px] text-fg hover:bg-panel-2"
          onClick={onBrowser}
        >
          <GlobeIcon size={14} /> Open in browser
        </button>
      </div>
    </div>
  );
}

/**
 * Phase 2 embedded artifact viewer (phone). Tapping a share link in chat opens this instead of
 * leaving the app. It fetches the bytes through the gateway/owning node over the mesh connection
 * (`remote_share_fetch`), so no LAN HTTP reachability is needed. HTML renders in a sandboxed
 * iframe (opaque origin, scripts blocked — the in-app equivalent of the host's `sandbox` CSP) and
 * images in an <img>; PDF/other types fall back to the system browser. Expired/revoked/offline
 * shares show a clear error state, never a blank screen.
 */
export function ShareViewer() {
  const url = useStore((s) => s.shareUrl);
  const close = () => useStore.getState().setShareUrl(null);
  if (!url) return null;
  return <ShareViewerInner key={url} url={url} onClose={close} />;
}

function ShareViewerInner({ url, onClose }: { url: string; onClose: () => void }) {
  const ref = parseShareUrl(url);
  const [busy, setBusy] = useState(true);
  const [fetched, setFetched] = useState<ShareFetch | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    setBusy(true);
    setError(null);
    setFetched(null);
    if (!ref) {
      setBusy(false);
      setError("not a share link");
      return;
    }
    api.shareFetch(ref.nodeId, ref.token).then(
      (f) => {
        if (!live) return;
        setBusy(false);
        setFetched(f);
      },
      (e) => {
        if (!live) return;
        setBusy(false);
        setError(errorText(e));
      },
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url, attempt]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const openBrowser = () => openExternal(url, { raw: true });
  const retry = () => setAttempt((n) => n + 1);
  const mime = (fetched && headerOf(fetched, "content-type")) || "application/octet-stream";
  const name = (fetched && filenameOf(fetched)) || "Shared file";
  const ok = !!fetched && fetched.status === 200;
  const isHtml = /^text\/html\b/i.test(mime);
  const isImage = /^image\//i.test(mime);

  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Shared file: ${name}`}
      className="fixed inset-0 z-50 flex flex-col bg-bg pb-[env(safe-area-inset-bottom)] pt-[env(safe-area-inset-top)]"
    >
      <div className="flex shrink-0 items-center gap-2 border-b border-line px-3 py-2">
        <FileTextIcon size={16} className="shrink-0 text-faint" />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-medium text-fg">{name}</div>
          <div className="truncate text-[11px] text-faint">{mime}</div>
        </div>
        <button
          type="button"
          className="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border px-2 text-[12px] text-muted hover:bg-panel-2 hover:text-fg"
          onClick={openBrowser}
          title="Open in the system browser"
        >
          <GlobeIcon size={14} /> <span className="hidden sm:inline">Open in browser</span>
        </button>
        <button
          type="button"
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-faint hover:bg-panel-2 hover:text-fg"
          onClick={onClose}
          title="Close"
          aria-label="Close"
        >
          <XIcon size={16} />
        </button>
      </div>

      <div className="relative min-h-0 flex-1">
        {busy && (
          <div className="flex h-full items-center justify-center text-[13px] text-muted">Loading artifact…</div>
        )}
        {!busy && error && (
          <Message
            title="Couldn't reach this file"
            body={`The host may be offline or out of range. ${error}`}
            onRetry={retry}
            onBrowser={openBrowser}
          />
        )}
        {!busy && !error && fetched && !ok && (
          <Message
            title="This link is no longer available"
            body="The share has expired, was revoked, or the link is wrong. Ask for a new link."
            onRetry={retry}
            onBrowser={openBrowser}
          />
        )}
        {!busy && !error && fetched && ok && isHtml && (
          // sandbox="" gives an opaque origin and blocks scripts/forms — the in-app equivalent of
          // the host's `Content-Security-Policy: sandbox` (no allow-same-origin).
          <iframe
            title={name}
            sandbox=""
            srcDoc={textOf(fetched.bodyB64)}
            className="h-full w-full border-0 bg-white"
          />
        )}
        {!busy && !error && fetched && ok && isImage && (
          <div className="flex h-full items-center justify-center overflow-auto p-3">
            <img
              src={`data:${mime};base64,${fetched.bodyB64}`}
              alt={name}
              className="max-h-full max-w-full object-contain"
            />
          </div>
        )}
        {!busy && !error && fetched && ok && !isHtml && !isImage && (
          <Message
            title="Can't show this file here"
            body={`${mime} isn't rendered in the app. Open it in your browser instead.`}
            onBrowser={openBrowser}
          />
        )}
      </div>

      <div className="flex shrink-0 items-center gap-2 border-t border-line px-3 py-2">
        <span className="text-[11px] text-faint">Viewed in an isolated sandbox; scripts are blocked.</span>
        <span className="flex-1" />
        <button
          type="button"
          className="inline-flex h-8 shrink-0 items-center gap-1 rounded-md px-2 text-[12px] text-accent-ink hover:bg-panel-2"
          onClick={openBrowser}
        >
          Open in browser <ArrowRightIcon size={13} />
        </button>
      </div>
    </div>,
    document.body,
  );
}
