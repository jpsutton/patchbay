import { useEffect, useRef, useState } from "react";
import type { Attachment } from "../lib/ipc";
import { AttachmentChip } from "./Composer";
import { insertNewlineAtCaret, isSendKey, needsExplicitNewline, useStore } from "../lib/store";

/** Inline editor for a previously sent user message. */
export function EditMessage({
  text,
  attachments,
  onCancel,
  onSubmit,
}: {
  text: string;
  attachments: Attachment[];
  onCancel: () => void;
  onSubmit: (text: string, attachments: Attachment[]) => void | Promise<void>;
}) {
  const [draft, setDraft] = useState(text);
  const [atts, setAtts] = useState(attachments);
  const ref = useRef<HTMLTextAreaElement>(null);
  const sendKey = useStore((s) => s.prefs.sendKey);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 320)}px`;
  }, [draft]);

  const canSend = draft.trim().length > 0 || atts.length > 0;
  return (
    <div data-amend="" className="w-full max-w-[85%] self-end rounded-2xl border border-accent bg-raised p-3 shadow-raised">
      {atts.length > 0 && (
        <div className="mb-2 flex flex-wrap gap-2">
          {atts.map((a) => (
            <AttachmentChip key={a.id} att={a} onRemove={() => setAtts((p) => p.filter((x) => x.id !== a.id))} />
          ))}
        </div>
      )}
      <textarea
        ref={ref}
        className="block w-full resize-none bg-transparent text-[15px] leading-6 outline-none"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") onCancel();
          if (e.nativeEvent.isComposing) return;
          if (isSendKey(e, sendKey) && canSend) {
            e.preventDefault();
            onSubmit(draft, atts);
            return;
          }
          if (needsExplicitNewline(e, sendKey)) {
            e.preventDefault();
            const el = e.currentTarget;
            const start = el.selectionStart ?? draft.length;
            const end = el.selectionEnd ?? start;
            const next = insertNewlineAtCaret(draft, start, end);
            setDraft(next.value);
            requestAnimationFrame(() => el.setSelectionRange(next.caret, next.caret));
          }
        }}
      />
      <div className="mt-2 flex justify-end gap-2">
        <button className="h-8 rounded-lg px-3 text-xs text-muted hover:bg-panel-2 hover:text-fg" onClick={onCancel}>
          Cancel
        </button>
        <button
          className="h-8 rounded-lg bg-accent px-3 text-xs font-semibold text-accent-fg disabled:opacity-40"
          disabled={!canSend}
          onClick={() => onSubmit(draft, atts)}
        >
          Resend
        </button>
      </div>
    </div>
  );
}
