import { useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorText, type AcpCommand, type Attachment } from "../lib/ipc";
import { forceSendKeyLabel, insertNewlineAtCaret, isForceSendKey, isSendKey, needsExplicitNewline, useStore } from "../lib/store";
import { ViewActive } from "../lib/shortcuts";
import { fuzzyPaths } from "../lib/fuzzy";
import { ArrowUpIcon, PaperclipIcon, SlidersIcon } from "./icons";
import { ChipRows } from "./ChipSelect";
import { isRemoteApp, isWeb } from "../lib/platform";

const MAX_ATTACHMENTS = 10;

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

function fileToBase64(f: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).split(",", 2)[1] ?? "");
    r.onerror = () => reject(r.error);
    r.readAsDataURL(f);
  });
}

/** Longest edge the backend sends a model (src-tauri/src/attachments.rs, MAX_IMAGE_EDGE). */
const MAX_IMAGE_EDGE = 2048;

/**
 * A phone photo shrunk to what the model would get anyway, before it goes over the network.
 * Returns the file unchanged when it isn't a larger still image.
 */
async function shrinkPhoto(f: File): Promise<{ blob: Blob; name: string }> {
  const keep = { blob: f as Blob, name: f.name || `photo-${Date.now()}.jpg` };
  if (!f.type.startsWith("image/") || f.type === "image/gif") return keep;
  const bmp = await createImageBitmap(f).catch(() => null);
  if (!bmp) return keep;
  const k = MAX_IMAGE_EDGE / Math.max(bmp.width, bmp.height);
  if (k >= 1) {
    bmp.close();
    return keep;
  }
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(bmp.width * k);
  canvas.height = Math.round(bmp.height * k);
  canvas.getContext("2d")?.drawImage(bmp, 0, 0, canvas.width, canvas.height);
  bmp.close();
  const blob = await new Promise<Blob | null>((r) => canvas.toBlob(r, "image/jpeg", 0.88));
  return blob ? { blob, name: keep.name.replace(/\.[^.]*$/, "") + ".jpg" } : keep;
}

export function AttachmentChip({ att, onRemove }: { att: Attachment; onRemove?: () => void }) {
  const [thumb, setThumb] = useState<string | null>(null);
  useEffect(() => {
    if (att.kind !== "image") return;
    let live = true;
    api
      .attachmentThumbnail(att.id)
      .then((t) => live && setThumb(t))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [att.id, att.kind]);
  const badge = att.kind === "pdf" ? "PDF" : att.kind === "text" ? "TXT" : "IMG";
  return (
    <div
      className="group relative flex max-w-56 items-center gap-2 rounded-[10px] border border-border bg-panel px-2 py-1.5 text-xs"
      title={`${att.name} · ${att.mime} · ${fmtSize(att.size)}`}
    >
      {thumb ? (
        <img src={thumb} alt="" className="h-8 w-8 rounded-md object-cover" />
      ) : (
        <span className="flex h-8 w-8 items-center justify-center rounded-md bg-panel-2 font-mono text-[10px] font-medium text-muted">
          {badge}
        </span>
      )}
      <div className="min-w-0">
        <div className="truncate text-fg">{att.name}</div>
        <div className="font-mono text-[11px] text-faint">{fmtSize(att.size)}</div>
      </div>
      {onRemove && (
        <button
          className="absolute -right-1.5 -top-1.5 hidden h-4 w-4 items-center justify-center rounded-full bg-fg text-[11px] leading-none text-bg group-hover:flex"
          onClick={onRemove}
          aria-label={`Remove ${att.name}`}
        >
          ×
        </button>
      )}
    </div>
  );
}

/** Commands matching the "/query" being typed: prefix matches first, then substring matches. */
function matchCommands(commands: AcpCommand[], text: string): AcpCommand[] {
  const m = /^\/(\S*)$/.exec(text);
  if (!m) return [];
  const q = m[1].toLowerCase();
  const prefix = commands.filter((c) => c.name.toLowerCase().startsWith(q));
  const rest = q ? commands.filter((c) => !c.name.toLowerCase().startsWith(q) && c.name.toLowerCase().includes(q)) : [];
  return [...prefix, ...rest];
}

export function Composer({
  onSend,
  onStop,
  busy,
  disabled,
  placeholder,
  footer,
  footerLabel = "Model and options",
  trailing,
  warnNoVision,
  commands,
  mentionFiles,
  onEditLast,
  sendWhileBusy,
  busySendHint,
  restore,
  onRestored,
  onForceSend,
}: {
  /** `mentions`: project-relative paths picked with @ that are still in the text. */
  onSend: (text: string, attachments: Attachment[], mentions: string[]) => void | Promise<void>;
  onStop?: () => void;
  busy?: boolean;
  disabled?: boolean;
  placeholder?: string;
  footer?: ReactNode;
  /** Names the phone's button and sheet that hold the footer. */
  footerLabel?: string;
  /** Shown just left of the send button, and never moved into the phone's sheet. */
  trailing?: ReactNode;
  warnNoVision?: boolean;
  /** Slash commands the agent advertises (Code tab). */
  commands?: AcpCommand[];
  /** Enables @-mentions of project files (Code tab). */
  mentionFiles?: () => Promise<string[]>;
  /** Up arrow in an empty composer. */
  onEditLast?: () => void;
  /** Sending stays open while `busy`, beside Stop (Code tab: the message steers or queues). */
  sendWhileBusy?: boolean;
  /** The send button's tooltip while `busy`. */
  busySendHint?: string;
  /** Text and attachments to take back into the composer (queued messages a stop dropped). */
  restore?: { text: string; attachments: Attachment[] } | null;
  /** Called once `restore` is in the composer. */
  onRestored?: () => void;
  /**
   * Send while `busy` by cancelling the running turn (Code tab). Bound to Ctrl+Enter, or
   * Ctrl+Shift+Enter when Ctrl+Enter is the send key; idle, those keys keep their usual meaning.
   */
  onForceSend?: (text: string, attachments: Attachment[], mentions: string[]) => void | Promise<void>;
}) {
  const [text, setText] = useState("");
  const [atts, setAtts] = useState<Attachment[]>([]);
  const [adding, setAdding] = useState(0);
  const [dragging, setDragging] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);
  const setToast = useStore((s) => s.setToast);
  const sendKey = useStore((s) => s.prefs.sendKey);
  const [menuIdx, setMenuIdx] = useState(0);
  const [menuDismissed, setMenuDismissed] = useState(false);
  const [caret, setCaret] = useState(0);
  const [mentions, setMentions] = useState<string[]>([]);
  const [files, setFiles] = useState<string[] | null>(null);
  const filesAt = useRef(0);
  const menuRef = useRef<HTMLDivElement>(null);
  // The phone keeps the footer's chips in a sheet behind one button; a row of them doesn't fit.
  const sheetFooter = isRemoteApp() && !!footer;
  const [optionsOpen, setOptionsOpen] = useState(false);

  // "@query" right before the caret, when mentions are on.
  const mention = useMemo(() => {
    if (!mentionFiles) return null;
    const m = /(^|\s)@([^\s@]*)$/.exec(text.slice(0, caret));
    return m ? { query: m[2], start: caret - m[2].length - 1 } : null;
  }, [mentionFiles, text, caret]);

  useEffect(() => {
    if (!mention || !mentionFiles) return;
    // The file list is cheap to fetch but not free; reuse it for 30 s.
    if (files && Date.now() - filesAt.current < 30_000) return;
    filesAt.current = Date.now();
    mentionFiles()
      .then(setFiles)
      .catch((e) => setToast(errorText(e)));
  }, [mention, mentionFiles, files, setToast]);

  type Item = { key: string; label: string; hint?: string; detail?: string; pick: () => void };
  const items = useMemo<Item[]>(() => {
    if (mention) {
      return fuzzyPaths(files ?? [], mention.query, 40).map((path) => ({
        key: path,
        label: path.slice(path.lastIndexOf("/") + 1),
        detail: path,
        pick: () => {
          const before = text.slice(0, mention.start);
          const after = text.slice(caret);
          const next = `${before}@${path} ${after.replace(/^\s+/, "")}`;
          setText(next);
          setMentions((m) => (m.includes(path) ? m : [...m, path]));
          const pos = before.length + path.length + 2;
          requestAnimationFrame(() => {
            ref.current?.setSelectionRange(pos, pos);
            setCaret(pos);
          });
        },
      }));
    }
    if (commands?.length) {
      return matchCommands(commands, text).map((c) => ({
        key: c.name,
        label: `/${c.name}`,
        hint: c.input?.hint,
        detail: c.description,
        pick: () => {
          setText(`/${c.name} `);
          setMenuDismissed(true);
        },
      }));
    }
    return [];
  }, [mention, files, commands, text, caret]);
  const menuOpen = items.length > 0 && !menuDismissed;

  useEffect(() => {
    setMenuIdx(0);
    if (!text.startsWith("/") && !mention) setMenuDismissed(false);
  }, [text, mention?.start]);

  useEffect(() => {
    menuRef.current?.querySelector(`[data-idx="${menuIdx}"]`)?.scrollIntoView({ block: "nearest" });
  }, [menuIdx]);

  const pickItem = (it: Item) => {
    it.pick();
    ref.current?.focus();
  };

  const addMany = async (jobs: (() => Promise<Attachment>)[]) => {
    const room = MAX_ATTACHMENTS - atts.length;
    if (jobs.length > room) setToast(`At most ${MAX_ATTACHMENTS} attachments per message`);
    const run = jobs.slice(0, Math.max(0, room));
    setAdding((n) => n + run.length);
    for (const job of run) {
      try {
        const a = await job();
        setAtts((prev) => [...prev, a]);
      } catch (e) {
        setToast(errorText(e));
      } finally {
        setAdding((n) => n - 1);
      }
    }
  };

  // Native file drops (Tauri intercepts them, so HTML5 drop events never see files).
  const addRef = useRef(addMany);
  addRef.current = addMany;
  const viewActive = useContext(ViewActive);
  const activeRef = useRef(viewActive);
  activeRef.current = viewActive;
  useEffect(() => {
    if (isWeb()) return webDrops();
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((ev) => {
        const p = ev.payload;
        if (!activeRef.current) return;
        if (p.type === "enter" || p.type === "over") setDragging(true);
        else if (p.type === "leave") setDragging(false);
        else if (p.type === "drop") {
          setDragging(false);
          addRef.current(p.paths.map((path) => () => api.addAttachmentPath(path)));
        }
      })
      .then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    if (!restore) return;
    // Taken back ahead of anything typed since: it was written first.
    setText((t) => (t.trim() ? `${restore.text}\n\n${t}` : restore.text));
    setAtts((p) => [...restore.attachments, ...p].slice(0, MAX_ATTACHMENTS));
    onRestored?.();
  }, [restore]);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 280)}px`;
  }, [text]);

  // In a browser, dropped files arrive as HTML5 drops carrying their contents, not local paths.
  const webDrops = () => {
    const files = (e: DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes("Files");
    const over = (e: DragEvent) => {
      if (!activeRef.current || !files(e)) return;
      e.preventDefault();
      setDragging(true);
    };
    const leave = (e: DragEvent) => {
      // Leaving the page, not just crossing between elements inside it.
      if (!e.relatedTarget) setDragging(false);
    };
    const drop = (e: DragEvent) => {
      if (!activeRef.current || !files(e)) return;
      e.preventDefault();
      setDragging(false);
      const list = Array.from(e.dataTransfer?.files ?? []);
      addRef.current(list.map((f) => async () => api.addAttachmentBytes(f.name, await fileToBase64(f))));
    };
    window.addEventListener("dragover", over);
    window.addEventListener("dragleave", leave);
    window.addEventListener("drop", drop);
    return () => {
      window.removeEventListener("dragover", over);
      window.removeEventListener("dragleave", leave);
      window.removeEventListener("drop", drop);
    };
  };

  const filesRef = useRef<HTMLInputElement>(null);
  const pick = async () => {
    // The phone app and a browser have no native file dialog; the web view's picker reaches the
    // photo library on a phone.
    if (isRemoteApp() || isWeb()) {
      filesRef.current?.click();
      return;
    }
    const sel = await open({ multiple: true, directory: false });
    if (!sel) return;
    const paths = Array.isArray(sel) ? sel : [sel];
    addMany(paths.map((p) => () => api.addAttachmentPath(p)));
  };

  const forcing = busy && !!onForceSend;
  const forceHint = forcing ? `${forceSendKeyLabel(sendKey)} sends now: stops the current turn` : null;

  const submit = async (force = false) => {
    if ((busy && !sendWhileBusy && !force) || disabled || adding > 0) return;
    if (!text.trim() && atts.length === 0) return;
    const t = text;
    const a = atts;
    // Only mentions still present in the text count.
    const m = mentions.filter((p) => t.includes(`@${p}`));
    setText("");
    setAtts([]);
    setMentions([]);
    await (force && onForceSend ? onForceSend : onSend)(t, a, m);
    ref.current?.focus();
  };

  const hasImage = atts.some((a) => a.kind === "image");
  const hasContent = !!text.trim() || atts.length > 0;

  return (
    <div
      className={`relative rounded-2xl border bg-raised shadow-raised transition-colors focus-within:border-fg/20 ${
        dragging ? "border-accent" : "border-border"
      }`}
    >
      {dragging && (
        <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center rounded-2xl bg-accent/10 text-sm font-medium text-accent-ink">
          Drop files to attach
        </div>
      )}
      {(atts.length > 0 || adding > 0) && (
        <div className="flex flex-wrap gap-2 px-3 pt-3">
          {atts.map((a) => (
            <AttachmentChip key={a.id} att={a} onRemove={() => setAtts((p) => p.filter((x) => x.id !== a.id))} />
          ))}
          {adding > 0 && <div className="self-center text-xs text-muted">Adding {adding}…</div>}
        </div>
      )}
      {menuOpen && (
        <div
          ref={menuRef}
          role="listbox"
          aria-label={mention ? "Project files" : "Slash commands"}
          className="absolute bottom-full left-0 right-0 z-20 mb-2 max-h-64 overflow-y-auto rounded-xl border border-border bg-raised p-1 text-xs shadow-float"
        >
          {items.map((it, i) => (
            <button
              key={it.key}
              data-idx={i}
              role="option"
              aria-selected={i === menuIdx}
              className={`flex w-full items-baseline gap-2 rounded-lg px-2.5 py-1.5 text-left ${i === menuIdx ? "bg-panel-2" : "hover:bg-panel"}`}
              onMouseDown={(e) => {
                e.preventDefault();
                pickItem(it);
              }}
              onMouseEnter={() => setMenuIdx(i)}
            >
              <span className="shrink-0 font-mono text-fg">{it.label}</span>
              {it.hint && <span className="shrink-0 font-mono text-muted">{it.hint}</span>}
              {it.detail && <span className="min-w-0 truncate text-muted">{it.detail}</span>}
            </button>
          ))}
        </div>
      )}
      {hasImage && warnNoVision && (
        <div className="px-3 pt-2 text-xs text-warn">This model is marked as not supporting images.</div>
      )}
      <textarea
        ref={ref}
        rows={1}
        value={text}
        disabled={disabled}
        data-composer
        placeholder={placeholder ?? "Message"}
        title={
          (isRemoteApp()
            ? "Enter for a new line; send with the button"
            : sendKey === "mod-enter"
              ? "Ctrl+Enter to send, Enter for a new line"
              : "Enter to send, Ctrl+Enter or Shift+Enter for a new line") + (forceHint ? `\n${forceHint}` : "")
        }
        className="block w-full resize-none bg-transparent px-4 pb-2 pt-3.5 text-[15px] leading-6 text-fg outline-none placeholder:text-faint"
        onChange={(e) => {
          setText(e.target.value);
          setCaret(e.target.selectionStart ?? e.target.value.length);
        }}
        onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
        onKeyDown={(e) => {
          if (menuOpen) {
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              const d = e.key === "ArrowDown" ? 1 : -1;
              setMenuIdx((i) => (i + d + items.length) % items.length);
              return;
            }
            if ((e.key === "Enter" || e.key === "Tab") && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              pickItem(items[menuIdx] ?? items[0]);
              return;
            }
            if (e.key === "Escape") {
              e.preventDefault();
              setMenuDismissed(true);
              return;
            }
          }
          if (e.key === "ArrowUp" && onEditLast && !text && atts.length === 0 && !e.shiftKey && !e.altKey) {
            e.preventDefault();
            onEditLast();
            return;
          }
          if (e.nativeEvent.isComposing) return;
          // Mid-turn only: idle, the same keys send or break the line as the preference says.
          if (forcing && isForceSendKey(e, sendKey)) {
            e.preventDefault();
            submit(true);
            return;
          }
          if (isSendKey(e, sendKey)) {
            e.preventDefault();
            submit();
            return;
          }
          if (needsExplicitNewline(e, sendKey)) {
            e.preventDefault();
            const el = e.currentTarget;
            const start = el.selectionStart ?? text.length;
            const end = el.selectionEnd ?? start;
            const next = insertNewlineAtCaret(text, start, end);
            setText(next.value);
            setCaret(next.caret);
            requestAnimationFrame(() => el.setSelectionRange(next.caret, next.caret));
          }
        }}
        onPaste={(e) => {
          const files = Array.from(e.clipboardData.files);
          if (files.length === 0) return;
          e.preventDefault();
          addMany(
            files.map((f, i) => async () => {
              const name = f.name || `pasted-${Date.now()}-${i}.${(f.type.split("/")[1] ?? "bin").replace("jpeg", "jpg")}`;
              return api.addAttachmentBytes(name, await fileToBase64(f));
            }),
          );
        }}
      />
      <div className="flex items-center gap-1.5 px-2.5 pb-2.5">
        <button
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded-[9px] border border-border text-muted hover:bg-panel-2/60 hover:text-fg disabled:opacity-40"
          onClick={pick}
          disabled={disabled || atts.length >= MAX_ATTACHMENTS}
          title="Attach files (images, PDFs, text)"
          aria-label="Attach files"
        >
          <PaperclipIcon size={16} />
        </button>
        {(isRemoteApp() || isWeb()) && (
          <input
            ref={filesRef}
            type="file"
            multiple
            className="hidden"
            onChange={(e) => {
              const files = Array.from(e.currentTarget.files ?? []);
              e.currentTarget.value = "";
              addMany(
                files.map((f) => async () => {
                  // Phones shrink camera photos before the upload; a browser sends files as they are.
                  const { blob, name } = isWeb() ? { blob: f, name: f.name } : await shrinkPhoto(f);
                  return api.addAttachmentBytes(name, await fileToBase64(blob));
                }),
              );
            }}
          />
        )}
        {sheetFooter ? (
          <>
            <button
              className="flex h-8 w-8 shrink-0 items-center justify-center rounded-[9px] border border-border text-muted hover:bg-panel-2/60 hover:text-fg"
              onClick={() => setOptionsOpen(true)}
              title={footerLabel}
              aria-label={footerLabel}
              aria-haspopup="dialog"
            >
              <SlidersIcon size={16} />
            </button>
            <div className="flex-1" />
          </>
        ) : (
          <div className="flex min-w-0 flex-1 items-center gap-1.5">{footer}</div>
        )}
        {trailing}
        {busy && (
          <button
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[10px] border border-border bg-panel-2 text-fg hover:bg-border"
            onClick={onStop}
            title="Stop (Esc)"
            aria-label="Stop"
          >
            <span className="h-3 w-3 rounded-[3px] bg-current" />
          </button>
        )}
        {(!busy || (sendWhileBusy && hasContent)) && (
          <button
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[10px] bg-accent text-accent-fg transition-colors hover:brightness-105 disabled:bg-panel-2 disabled:text-faint"
            onClick={() => submit()}
            disabled={disabled || adding > 0 || !hasContent}
            title={busy ? [busySendHint ?? "Send", forceHint].filter(Boolean).join("\n") : "Send"}
            aria-label="Send"
          >
            <ArrowUpIcon size={18} strokeWidth={2.4} />
          </button>
        )}
      </div>
      {sheetFooter &&
        optionsOpen &&
        createPortal(
          <div className="fixed inset-0 z-[60] flex flex-col justify-end bg-black/45" onClick={() => setOptionsOpen(false)}>
            <div
              role="dialog"
              aria-label={footerLabel}
              className="rounded-t-2xl border-t border-border bg-raised px-2 pb-[calc(env(safe-area-inset-bottom)+8px)] pt-2"
              onClick={(e) => e.stopPropagation()}
            >
              <div className="px-3 pb-1 pt-1 text-[13px] text-muted">{footerLabel}</div>
              <ChipRows.Provider value={true}>
                <div className="flex flex-col divide-y divide-line">{footer}</div>
              </ChipRows.Provider>
              <button
                className="mt-1 flex h-12 w-full items-center justify-center rounded-xl text-[16px] text-muted active:bg-panel-2"
                onClick={() => setOptionsOpen(false)}
              >
                Done
              </button>
            </div>
          </div>,
          document.body,
        )}
    </div>
  );
}
