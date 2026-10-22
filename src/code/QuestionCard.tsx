import { useState } from "react";
import { api, errorText, type AgentQuestion, type AgentQuestionAnswer, type AgentQuestionForm } from "../lib/ipc";
import { useStore } from "../lib/store";

/**
 * One agent's questions, one at a time. Skip leaves that question out. Something else is an
 * "Other" answer plus the text. The card stays until Send; nothing here dismisses it.
 */
export function QuestionCard({
  requestId,
  form,
  sessionId,
}: {
  requestId: string;
  form: AgentQuestionForm;
  sessionId: string;
}) {
  const questions = form.questions;
  const [index, setIndex] = useState(0);
  const [choices, setChoices] = useState<Record<string, { picks: string[]; note: string; other: boolean; skipped: boolean }>>({});
  const [sending, setSending] = useState(false);
  const q = questions[index];
  if (!q) return null;
  const choice = choices[q.id] ?? { picks: [], note: "", other: false, skipped: false };
  const set = (next: typeof choice) => setChoices((all) => ({ ...all, [q.id]: next }));
  const last = index === questions.length - 1;
  const ready = choice.skipped || choice.picks.length > 0 || (choice.other && choice.note.trim().length > 0);

  const pick = (id: string) => {
    if (q.multi) {
      const has = choice.picks.includes(id);
      set({ ...choice, skipped: false, picks: has ? choice.picks.filter((p) => p !== id) : [...choice.picks, id] });
      return;
    }
    set({ ...choice, skipped: false, other: false, picks: [id] });
  };

  const answerOf = (question: AgentQuestion): AgentQuestionAnswer | null => {
    const c = choices[question.id];
    if (!c || c.skipped) return null;
    return { id: question.id, picks: c.other && !question.multi ? [] : c.picks, note: c.other ? c.note.trim() : "" };
  };

  const send = async (withCurrentSkipped: boolean) => {
    if (sending) return;
    const current = withCurrentSkipped ? { ...choice, skipped: true, picks: [], other: false } : choice;
    if (withCurrentSkipped) set(current);
    const answers = questions.flatMap((question) => {
      if (question.id === q.id) {
        if (current.skipped) return [];
        const note = current.other ? current.note.trim() : "";
        if (current.picks.length === 0 && !note) return [];
        return [{ id: question.id, picks: current.other && !question.multi ? [] : current.picks, note }];
      }
      const a = answerOf(question);
      return a && (a.picks.length > 0 || a.note) ? [a] : [];
    });
    setSending(true);
    try {
      await api.codeQuestionRespond(requestId, answers, sessionId);
    } catch (e) {
      setSending(false);
      useStore.getState().setToast(errorText(e));
    }
  };

  const goNext = () => {
    if (!ready || sending) return;
    if (last) {
      void send(false);
      return;
    }
    setIndex(index + 1);
  };

  return (
    <section aria-label="Question" className="flex flex-col gap-3 rounded-xl border border-accent/45 bg-accent/[0.06] p-3.5">
      <div className="flex items-center gap-2">
        <span className="signal-pulse h-2 w-2 rounded-full bg-accent" />
        <span className="text-xs font-medium text-accent-ink">Question</span>
      </div>
      <div className="flex items-start gap-3">
        <p className="min-w-0 flex-1 text-[15px] leading-6">{q.prompt}</p>
        {questions.length > 1 && (
          <div className="flex shrink-0 items-center gap-1 text-[12px] text-muted">
            <button
              type="button"
              className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 disabled:opacity-30"
              disabled={index === 0}
              aria-label="Previous question"
              onClick={() => setIndex(index - 1)}
            >
              ‹
            </button>
            <span className="tabular-nums">
              {index + 1} of {questions.length}
            </span>
            <button
              type="button"
              className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-panel-2 disabled:opacity-30"
              disabled={last}
              aria-label="Next question"
              onClick={() => setIndex(index + 1)}
            >
              ›
            </button>
          </div>
        )}
      </div>
      <div className="flex flex-col">
        {q.options.map((o, i) => {
          const on = choice.picks.includes(o.id) && !choice.skipped;
          return (
            <button
              key={o.id}
              type="button"
              aria-pressed={on}
              className={`flex items-start gap-3 border-t border-line py-3 text-left ${on ? "text-fg" : "text-muted"}`}
              onClick={() => pick(o.id)}
            >
              <span
                className={`mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[12px] ${on ? "bg-accent text-accent-fg" : "bg-panel-2 text-faint"}`}
              >
                {i + 1}
              </span>
              <span className="min-w-0">
                <span className="block text-[15px] text-fg">{o.label}</span>
                {o.description && <span className="mt-0.5 block text-[13px] leading-5 text-muted">{o.description}</span>}
              </span>
            </button>
          );
        })}
        {q.freeText && (
          <div className="flex items-start gap-3 border-t border-line py-3">
            <button
              type="button"
              aria-pressed={choice.other}
              className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-panel-2 text-[12px] text-faint"
              onClick={() => set({ ...choice, other: !choice.other, skipped: false, picks: q.multi ? choice.picks : [] })}
            >
              ✎
            </button>
            {choice.other ? (
              <textarea
                value={choice.note}
                rows={2}
                placeholder="Something else"
                className="min-w-0 flex-1 resize-none bg-transparent text-[15px] leading-6 text-fg outline-none placeholder:text-faint"
                onChange={(e) => set({ ...choice, note: e.target.value, other: true, skipped: false, picks: q.multi ? choice.picks : [] })}
              />
            ) : (
              <button
                type="button"
                className="text-[15px] text-muted"
                onClick={() => set({ ...choice, other: true, skipped: false, picks: q.multi ? choice.picks : [] })}
              >
                Something else
              </button>
            )}
          </div>
        )}
      </div>
      <div className="flex items-center justify-end gap-2">
        {q.skippable && (
          <button
            type="button"
            className="h-9 rounded-full border border-line px-4 text-[13px] text-fg hover:bg-panel-2 disabled:opacity-40"
            disabled={sending}
            onClick={() => {
              set({ picks: [], note: "", other: false, skipped: true });
              if (last) void send(true);
              else setIndex(index + 1);
            }}
          >
            Skip
          </button>
        )}
        <button
          type="button"
          className="h-9 rounded-full bg-accent px-4 text-[13px] font-semibold text-accent-fg disabled:opacity-40"
          disabled={!ready || sending}
          onClick={goNext}
        >
          {last ? "Send" : "Next"}
        </button>
      </div>
    </section>
  );
}
