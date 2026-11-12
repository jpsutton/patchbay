import { useEffect, useState } from "react";
import { api, errorText, type TeamBoard as Board, type TeamDecision } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";
import { splitTeamDecisions } from "./teamBoardDecisions";

const STATUS_LABEL: Record<TeamDecision["status"], string> = {
  awaiting_advisor: "waiting for the advisor",
  awaiting_user: "waiting for you",
  cleared: "cleared by the advisor",
  resolved: "answered",
};

/** The team's board (items by status), the decisions queue and recent events. Polls while open. */
export function TeamBoard({ teamId }: { teamId: string }) {
  const setToast = useStore((s) => s.setToast);
  const [board, setBoard] = useState<Board | null>(null);
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const load = () => api.teamBoard(teamId).then(setBoard, () => {});
  useEffect(() => {
    let live = true;
    const tick = () => api.teamBoard(teamId).then((b) => live && setBoard(b), () => {});
    tick();
    const t = setInterval(tick, 3000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [teamId]);
  if (!board) return null;

  const { waiting, other } = splitTeamDecisions(board.decisions);
  const answer = (d: TeamDecision, text: string) =>
    api.teamDecisionAnswer(teamId, d.id, text).then(() => { setAnswers((a) => ({ ...a, [d.id]: "" })); return load(); }, (e) => setToast(errorText(e)));

  return (
    <div className="flex flex-col gap-3 text-xs">
      <div>
        <div className="mb-1 font-medium">Open decisions · {waiting.length}</div>
        {waiting.length === 0 && <div className="text-muted">Nothing waiting.</div>}
        {waiting.map((d) => (
          <div key={d.id} className={`mb-2 rounded-md border p-2 ${d.disagrees ? "border-yellow-500" : "border-border"}`}>
            <div className="font-medium">{d.id}{d.itemId ? ` · ${d.itemId}` : ""}: {d.question}</div>
            <div className="text-muted">{STATUS_LABEL[d.status]}</div>
            {d.disagrees && <div className="my-1 rounded bg-yellow-500/20 px-1.5 py-0.5 font-medium">The advisor disagrees with the coordinator.</div>}
            <div className="mt-1 grid grid-cols-2 gap-2">
              <div className="rounded bg-panel-2 p-1.5">
                <div className="text-muted">Coordinator recommends</div>
                <div>{d.recommendation ?? "(nothing given)"}</div>
              </div>
              <div className="rounded bg-panel-2 p-1.5">
                <div className="text-muted">Advisor verdict</div>
                <div>{d.advisorVerdict ?? "(none, class " + d.class + ")"}{d.advisorChoice ? `: ${d.advisorChoice}` : ""}</div>
                {d.advisorText && <div className="text-muted">{d.advisorText}</div>}
              </div>
            </div>
            <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
              {d.options.map((o) => (
                <button key={o} className="rounded-md border border-border px-2 py-1 hover:border-accent" onClick={() => answer(d, o)}>{o}</button>
              ))}
              <input className={`${input} min-w-40 flex-1`} placeholder="Or answer in your own words" value={answers[d.id] ?? ""} onChange={(e) => setAnswers({ ...answers, [d.id]: e.target.value })} />
              <button className="rounded-md bg-accent px-2 py-1 font-medium text-accent-fg disabled:opacity-40" disabled={!(answers[d.id] ?? "").trim()} onClick={() => answer(d, answers[d.id])}>Answer</button>
            </div>
          </div>
        ))}
        {other.length > 0 && (
          <details className="text-muted">
            <summary className="cursor-pointer">Other decisions · {other.length}</summary>
            {other.map((d) => (
              <div key={d.id} className="py-0.5">
                {d.id}: {d.question} — {STATUS_LABEL[d.status]}
                {d.advisorVerdict ? `; advisor ${d.advisorVerdict}${d.advisorChoice ? ` (${d.advisorChoice})` : ""}` : ""}
                {d.userAnswer ? `; you: ${d.userAnswer}` : ""}
                {d.disagrees ? "; advisor disagreed with the coordinator" : ""}
              </div>
            ))}
          </details>
        )}
      </div>
      <div>
        <div className="mb-1 font-medium">Board · {board.items.length} items</div>
        {board.items.length === 0 && <div className="text-muted">No items yet. The coordinator adds them with item_add.</div>}
        <div className="grid grid-cols-2 gap-2">
          {board.statuses.filter((st) => board.items.some((i) => i.status === st)).map((st) => (
            <div key={st} className="rounded-md border border-border p-2">
              <div className="mb-1 font-mono text-[11px] uppercase text-muted">{st}</div>
              {board.items.filter((i) => i.status === st).map((i) => (
                <div key={i.id} className="py-0.5">
                  <span className="font-mono text-muted">{i.id}</span> {i.title}
                  <span className="text-muted"> · {i.type} · {i.severity}</span>
                </div>
              ))}
            </div>
          ))}
        </div>
      </div>
      {board.events.length > 0 && (
        <details className="text-muted">
          <summary className="cursor-pointer">Recent events · {board.events.length}</summary>
          {[...board.events].reverse().map((e) => (
            <div key={e.id} className="py-0.5">
              <span className="font-mono">{new Date(e.at).toLocaleTimeString()}</span> {e.itemId} {e.kind}: {e.text}
            </div>
          ))}
        </details>
      )}
    </div>
  );
}
