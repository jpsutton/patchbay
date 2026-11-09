import { useState, type MouseEvent } from "react";
import { api, errorText, type ShareInfo } from "../lib/ipc";
import { isDesktopApp } from "../lib/platform";
import { openExternal } from "../lib/external";
import { useStore } from "../lib/store";
import { copyText } from "./CopyButton";
import { ArrowRightIcon, ChatIcon, CopyIcon, GlobeIcon, TrashIcon } from "./icons";

/**
 * The explicit SHARE action on a generated-file row. Creating a share yields a URL rooted at the
 * per-node `artifacts.baseUrl` setting; the resulting link opens the artifact in the SYSTEM BROWSER
 * from an isolated, sandboxed origin (see src-tauri/shares.rs). Desktop-only for Phase 1 (loopback /
 * same-host reachability); the URL base is configurable so Phase 2 can proxy it through the gateway.
 *
 * Controls are rendered in normal flow (no floating popover) so they are never clipped by the
 * `overflow-hidden` of tool-call groups or virtualized transcript rows. Shares are read-only,
 * time-limited and revocable, and expose only the one chosen file.
 */
export function FileShare({ path, cwd }: { path: string; cwd: string | null }) {
  const [share, setShare] = useState<ShareInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!isDesktopApp()) return null;

  const create = async (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (share || busy) return;
    setBusy(true);
    setError(null);
    try {
      setShare(await api.createShare(path, cwd));
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (share) {
      try {
        await api.revokeShare(share.token);
      } catch {
        // The share is still cleared locally; an expired/forgotten token is already harmless.
      }
    }
    setShare(null);
  };

  const expires = share ? new Date(share.expiresAt).toLocaleString() : "";

  /** Put the existing share into the current chat draft as a clickable markdown link. */
  const addToChat = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (!share) return;
    // Escape the label so a filename with markdown punctuation can't break the link.
    const label = share.name.replace(/[\\\[\]]/g, (c) => `\\${c}`).replace(/\r?\n/g, " ");
    useStore.getState().insertIntoComposer(`[${label}](${share.url})`);
    useStore.getState().setToast(`Added ${share.name} link to your chat draft`);
  };

  return (
    <span
      className="ml-1 inline-flex shrink-0 items-center gap-1 align-middle font-sans"
      onClick={(e) => e.stopPropagation()}
    >
      <button
        type="button"
        className="inline-flex h-5 items-center gap-1 rounded border border-border px-1.5 text-[11px] text-muted hover:border-accent/50 hover:text-fg"
        title="Share this file with a link"
        aria-label="Share this file with a link"
        onClick={create}
        disabled={busy || !!share}
      >
        <GlobeIcon size={11} />
        {busy ? "Sharing…" : share ? "Shared" : "Share"}
      </button>
      {error && <span className="text-[11px] text-danger">Could not share: {error}</span>}
      {share && (
        <>
          <a
            href={share.url}
            title={`${share.url}\nExpires ${expires}. Anyone with the link can view this one file until then.`}
            className="inline-flex items-center gap-1 text-[11px] font-medium text-accent-ink underline decoration-accent/40 hover:decoration-accent"
            onClick={(e) => {
              e.preventDefault();
              openExternal(share.url); // system browser on desktop
            }}
          >
            Open {share.name} in browser
            <ArrowRightIcon size={12} />
          </a>
          <button
            type="button"
            className="inline-flex h-5 w-5 items-center justify-center rounded text-faint hover:bg-panel-2 hover:text-fg"
            onClick={() => void copyText(share.url)}
            title="Copy link"
            aria-label="Copy link"
          >
            <CopyIcon size={12} />
          </button>
          <button
            type="button"
            className="inline-flex h-5 items-center gap-1 rounded border border-border px-1.5 text-[11px] text-muted hover:border-accent/50 hover:text-fg"
            onClick={addToChat}
            title="Add the share link to your chat draft as markdown"
            aria-label="Add share link to chat draft"
          >
            <ChatIcon size={11} /> Add to chat
          </button>
          <button
            type="button"
            className="inline-flex h-5 w-5 items-center justify-center rounded text-faint hover:bg-panel-2 hover:text-danger"
            onClick={revoke}
            title="Revoke this link now"
            aria-label="Revoke this link"
          >
            <TrashIcon size={12} />
          </button>
        </>
      )}
    </span>
  );
}
