import { useEffect, useState } from "react";
import { AGENT_LABELS, api, errorText, type AgentId, type Team, type TeamDetail, type TeamTemplate } from "../lib/ipc";
import { useStore } from "../lib/store";
import { restartTeamMember } from "../lib/teamRoles";
import { Field, input } from "./ProviderPage";
import { RolesEditor, TemplatesEditor } from "./TeamEditors";
import { TeamBoard } from "./TeamBoard";

const DOT: Record<string, string> = { running: "bg-accent", waiting: "bg-yellow-500", stalled: "bg-red-500", idle: "bg-muted", stopped: "bg-border" };

const changed = () => {
  window.dispatchEvent(new Event("teams-changed"));
  void useStore.getState().refreshCodeSessions().catch(() => {});
};

/** Teams: Code sessions in named roles on one project (docs/design/teams.md), with the role and template editors. */
export function TeamsPage() {
  const [tab, setTab] = useState<"teams" | "roles" | "templates">("teams");
  return (
    <div className="max-w-3xl">
      <div className="mb-3 flex gap-1 text-xs">
        {(["teams", "roles", "templates"] as const).map((t) => (
          <button key={t} className={`rounded-md px-2.5 py-1 capitalize ${tab === t ? "bg-panel-2 font-medium" : "text-muted hover:bg-panel-2/60"}`} onClick={() => setTab(t)}>
            {t}
          </button>
        ))}
      </div>
      {tab === "teams" && <TeamsPanel />}
      {tab === "roles" && <RolesEditor />}
      {tab === "templates" && <TemplatesEditor />}
    </div>
  );
}

const SERVER_ERROR_MAP: { match: string; field: string }[] = [
  { match: "export folder", field: "exportDir" },
  { match: "export_dir", field: "exportDir" },
  { match: "worktree", field: "worktree" },
  { match: "pr_base", field: "pr_base" },
  { match: "branch", field: "pr_base" },
  { match: "id prefix", field: "id_prefix" },
  { match: "id_prefix", field: "id_prefix" },
  { match: "slug", field: "slug" },
  { match: "folder", field: "cwd" },
];

function TeamsPanel() {
  const setToast = useStore((s) => s.setToast);
  const sessions = useStore((s) => s.codeSessions);
  const [templates, setTemplates] = useState<TeamTemplate[]>([]);
  const [teams, setTeams] = useState<Team[]>([]);
  const [sel, setSel] = useState<string | "new" | null>(null);
  const [detail, setDetail] = useState<TeamDetail | null>(null);
  const [draft, setDraft] = useState({ templateId: "", slug: "", cwd: "", projectRules: "", exportDir: "" });
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [genericError, setGenericError] = useState<string | null>(null);
  const [touched, setTouched] = useState<Record<string, boolean>>({});
  const [pick, setPick] = useState<Record<string, string>>({});
  const fail = (e: unknown) => setToast(errorText(e));

  const slugTrim = draft.slug.trim();
  const slugErr = slugTrim
    ? (!/^[a-zA-Z0-9_-]+$/.test(slugTrim) ? "the slug may use letters, digits, - and _ only" : null)
    : (touched.slug ? "Team name is required" : null);
  const cwdErr = touched.cwd && !draft.cwd.trim() ? "Project folder is required" : null;
  const isFormValid =
    slugTrim.length > 0 &&
    /^[a-zA-Z0-9_-]+$/.test(slugTrim) &&
    draft.cwd.trim().length > 0 &&
    draft.templateId.length > 0 &&
    !slugErr &&
    !cwdErr;

  const reload = async () => {
    const [ts, tm] = await Promise.all([api.listTeamTemplates(), api.listTeams()]);
    setTemplates(ts);
    setTeams(tm.filter((t) => t.archivedAt === null));
    setDraft((d) => ({ ...d, templateId: d.templateId || ts[0]?.id || "" }));
  };
  useEffect(() => {
    reload().catch(fail);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The roster's activity is live, so poll the open team.
  useEffect(() => {
    if (!sel || sel === "new") {
      setDetail(null);
      return;
    }
    let live = true;
    const load = () => api.teamDetail(sel).then((d) => live && setDetail(d), () => {});
    load();
    const t = setInterval(load, 3000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [sel]);

  const refresh = () => (sel && sel !== "new" ? api.teamDetail(sel).then(setDetail, fail) : undefined);

  return (
    <div className="flex gap-4 pb-4">
      <div className="flex w-44 shrink-0 flex-col gap-0.5">
        {teams.map((t) => (
          <button key={t.id} className={`truncate rounded-md px-2 py-1.5 text-left text-xs ${sel === t.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`} onClick={() => setSel(t.id)}>
            {t.slug}
          </button>
        ))}
        {teams.length === 0 && <div className="px-2 py-1.5 text-xs text-muted">No teams</div>}
        <button
          className="mt-2 rounded-md border border-dashed border-border px-2 py-1.5 text-left text-xs text-muted hover:border-accent hover:text-fg"
          onClick={() => {
            setSel("new");
            setFieldErrors({});
            setGenericError(null);
            setTouched({});
          }}
        >
          + New team
        </button>
      </div>
      <div className="min-w-0 flex-1">
        {sel === "new" && (
          <div className="flex flex-col gap-2">
            {genericError && (
              <div className="rounded border border-danger/40 bg-danger/10 p-2 text-xs text-danger" role="alert">
                {genericError}
              </div>
            )}
            <Field label="Template">
              <select className={input} value={draft.templateId} onChange={(e) => setDraft({ ...draft, templateId: e.target.value })}>
                {templates.map((t) => (
                  <option key={t.id} value={t.id}>{t.name}</option>
                ))}
              </select>
            </Field>
            <Field label="Team name">
              <input
                className={input}
                placeholder="letters, digits, - _"
                value={draft.slug}
                onChange={(e) => {
                  setDraft({ ...draft, slug: e.target.value });
                  setTouched((t) => ({ ...t, slug: true }));
                  setFieldErrors((f) => ({ ...f, slug: "" }));
                  setGenericError(null);
                }}
              />
              {(fieldErrors.slug || slugErr) && (
                <span className="text-[11px] text-danger">{fieldErrors.slug || slugErr}</span>
              )}
            </Field>
            <Field label="Project folder">
              <input
                className={input}
                placeholder="e.g. /home/me/project"
                value={draft.cwd}
                onChange={(e) => {
                  setDraft({ ...draft, cwd: e.target.value });
                  setTouched((t) => ({ ...t, cwd: true }));
                  setFieldErrors((f) => ({ ...f, cwd: "" }));
                  setGenericError(null);
                }}
              />
              {(fieldErrors.cwd || cwdErr) && (
                <span className="text-[11px] text-danger">{fieldErrors.cwd || cwdErr}</span>
              )}
            </Field>
            <Field label="Export folder (optional)" hint="leave empty for none">
              <input
                className={input}
                placeholder="leave empty for none"
                value={draft.exportDir}
                onChange={(e) => {
                  setDraft({ ...draft, exportDir: e.target.value });
                  setFieldErrors((f) => ({ ...f, exportDir: "" }));
                  setGenericError(null);
                }}
              />
              {fieldErrors.exportDir && (
                <span className="text-[11px] text-danger">{fieldErrors.exportDir}</span>
              )}
            </Field>
            <Field label="Project rules" hint="Given to every member">
              <textarea className={`${input} h-28 resize-y font-mono text-xs`} placeholder="e.g. set CARGO_TARGET_DIR" value={draft.projectRules} onChange={(e) => setDraft({ ...draft, projectRules: e.target.value })} />
            </Field>
            <button
              className="self-start rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40"
              disabled={!isFormValid}
              onClick={async () => {
                try {
                  setFieldErrors({});
                  setGenericError(null);
                  const exportDir = draft.exportDir.trim() || undefined;
                  const t = await api.createTeam({ ...draft, slug: slugTrim, cwd: draft.cwd.trim(), exportDir });
                  await reload();
                  setSel(t.id);
                } catch (e) {
                  const msg = errorText(e);
                  const lower = msg.toLowerCase();
                  const entry = SERVER_ERROR_MAP.find((m) => lower.includes(m.match));
                  if (entry && (entry.field === "slug" || entry.field === "cwd" || entry.field === "exportDir")) {
                    setFieldErrors({ [entry.field]: msg });
                  } else {
                    setGenericError(msg);
                    fail(e);
                  }
                }
              }}
            >
              Create team
            </button>
          </div>
        )}
        {detail && sel !== "new" && (
          <div className="flex flex-col gap-3 text-xs">
            <div className="text-muted">{detail.team.cwd} · {detail.template.name} · {detail.team.exportDir ? `export to ${detail.team.exportDir}` : "export: off"}</div>
            {detail.template.seats.map((seat) => {
              const here = detail.members.filter((m) => m.member.seat === seat.seat);
              const role = detail.roles.find((r) => r.id === seat.roleId)?.name ?? seat.roleId;
              const taken = new Set(detail.members.map((m) => m.member.sessionId));
              const free = sessions
                .filter((s) => !taken.has(s.id) && s.archivedAt == null)
                .sort((a, b) => (b.agent === seat.agent ? 1 : 0) - (a.agent === seat.agent ? 1 : 0));
              return (
                <div key={seat.seat} className="rounded-md border border-border p-2">
                  <div className="mb-1 flex items-baseline gap-2">
                    <span className="rounded bg-panel-2 px-1.5 py-0.5 font-medium">{role}</span>
                    <span className="text-muted">{seat.seat} · {here.length}/{seat.count} · suggested {seat.agent}{seat.model ? ` ${seat.model}` : ""}</span>
                  </div>
                  {here.map((m) => {
                    const isBusy = m.activity === "running" || m.activity === "waiting" || m.activity === "stalled";
                    return (
                      <div key={m.member.sessionId} className="flex items-center gap-2 py-0.5">
                        <span className={`h-2 w-2 rounded-full ${DOT[m.activity ?? ""] ?? "bg-border"}`} />
                        <span className="truncate">{m.title ?? "(session gone)"}</span>
                        <span className="text-muted">{m.agent}{m.model ? ` · ${m.model}` : ""} · {m.activity ?? "other computer"}</span>
                        <div className="ml-auto flex items-center gap-2">
                          <button
                            className="text-muted hover:text-fg disabled:opacity-40"
                            disabled={isBusy}
                            title={isBusy ? "Session is mid-turn" : undefined}
                            onClick={async () => {
                              try {
                                const ran = await restartTeamMember(m.member.sessionId, role, detail.team.id);
                                if (ran) await refresh();
                              } catch (e) {
                                fail(e);
                              }
                            }}
                          >
                            Restart with fresh context
                          </button>
                          <button className="text-muted hover:text-fg" onClick={() => api.teamDetach(detail.team.id, m.member.sessionId).then(() => { changed(); return refresh(); }, fail)}>Remove</button>
                        </div>
                      </div>
                    );
                  })}
                  {here.length < seat.count && (
                    <div className="mt-1 flex flex-wrap gap-2">
                      <button
                        className="rounded-md border border-border px-2 text-xs hover:border-accent"
                        onClick={async () => {
                          try {
                            const s = await api.createCodeSession(
                              detail.team.cwd,
                              seat.model || null,
                              seat.agent as AgentId,
                            );
                            await api.teamAttach(detail.team.id, s.id, seat.seat);
                            changed();
                            await refresh();
                          } catch (e) {
                            fail(e);
                          }
                        }}
                      >
                        Start {(AGENT_LABELS as Record<string, string>)[seat.agent] ?? seat.agent} session
                      </button>
                      <select className={`${input} flex-1`} value={pick[seat.seat] ?? ""} onChange={(e) => setPick({ ...pick, [seat.seat]: e.target.value })}>
                        <option value="">Add an existing session…</option>
                        {free.map((s) => (
                          <option key={s.id} value={s.id}>{s.title} ({s.agent})</option>
                        ))}
                      </select>
                      <button
                        className="rounded-md border border-border px-2 text-xs disabled:opacity-40"
                        disabled={!pick[seat.seat]}
                        onClick={() => api.teamAttach(detail.team.id, pick[seat.seat], seat.seat).then(() => { setPick({ ...pick, [seat.seat]: "" }); changed(); return refresh(); }, fail)}
                      >
                        Add
                      </button>
                    </div>
                  )}
                </div>
              );
            })}
            <TeamBoard teamId={detail.team.id} />
            <div className="text-muted">Role instructions reach a member when its agent starts (Claude Code, Codex, Antigravity, Grok) or on each message from a teammate (other agents). Restart a running session to apply a new role.</div>
            <button
              className="self-start text-muted hover:text-fg"
              onClick={() => api.teamArchive(detail.team.id).then(() => { setSel(null); changed(); return reload(); }, fail)}
            >
              Archive team
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
