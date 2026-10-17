import { useEffect, useRef, useState } from "react";
import { api } from "./lib/ipc";
import { isSendKey, useStore } from "./lib/store";

/** The small always-on-top window the global shortcut opens; sends become new chats. */
export function QuickEntry() {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);
  const sendKey = useStore((s) => s.prefs.sendKey);

  useEffect(() => {
    useStore.getState().loadPrefs().catch(() => {});
    const focus = () => ref.current?.focus();
    focus();
    window.addEventListener("focus", focus);
    return () => window.removeEventListener("focus", focus);
  }, []);

  const submit = async () => {
    const t = text.trim();
    if (!t) return;
    setText("");
    await api.quickSubmit(t);
  };

  return (
    <div className="flex h-full flex-col justify-center bg-bg p-3">
      <div className="rounded-xl border border-border bg-panel px-3 py-2 shadow-sm focus-within:border-accent">
        <textarea
          ref={ref}
          rows={2}
          className="block w-full resize-none bg-transparent text-[15px] leading-6 text-fg outline-none placeholder:text-muted"
          placeholder="Ask anything; it opens as a new chat"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              setText("");
              api.quickDismiss();
            } else if (isSendKey(e, sendKey) && !e.nativeEvent.isComposing) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <div className="flex justify-between text-[12px] text-muted">
          <span>Patchbay</span>
          <span>{sendKey === "mod-enter" ? "Ctrl+Enter" : "Enter"} to send · Esc to close</span>
        </div>
      </div>
    </div>
  );
}
