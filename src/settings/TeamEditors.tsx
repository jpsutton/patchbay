import { useEffect, useState } from "react";
import { AGENT_LABELS, api, errorText, usesPatchbayModels, type TeamCaps, type TeamRole, type TeamSeat, type TeamTemplate } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";

function SeatModelPicker({
  agent,
  value,
  readOnly,
  onChange,
}: {
  agent: string;
  value: string | null;
  readOnly?: boolean;
  onChange: (v: string | null) => void;
}) {
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const code = useStore((s) => s.code);
  const codeSessions = useStore((s) => s.codeSessions);

  if (usesPatchbayModels(agent)) {
    if (models.length === 0) {
      return (
        <input
          className={input}
          placeholder="Model (optional)"
          value={value ?? ""}
          readOnly={readOnly}
          onChange={(e) => onChange(e.target.value || null)}
        />
      );
    }
    return (
      <select
        className={input}
        value={value ?? ""}
        disabled={readOnly}
        aria-label="Model"
        onChange={(e) => onChange(e.target.value || null)}
      >
        <option value="">Default model</option>
        {value && !models.some((m) => m.id === value) && (
          <option value={value}>{value}</option>
        )}
        {providers.map((p) => {
          const ms = models.filter((m) => m.providerId === p.id);
          if (ms.length === 0) return null;
          return (
            <optgroup key={p.id} label={p.name}>
              {ms.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.displayName}
                </option>
              ))}
            </optgroup>
          );
        })}
      </select>
    );
  }

  // Other agents (claude-code, codex, cursor, etc.): check store session cache
  const cachedModels = (() => {
    for (const s of codeSessions) {
      if (s.agent === agent) {
        const av = code[s.id]?.models?.availableModels;
        if (av && av.length > 0) return av;
      }
    }
    for (const [id, c] of Object.entries(code)) {
      const av = c.models?.availableModels;
      if (av && av.length > 0) {
        const sess = codeSessions.find((s) => s.id === id);
        if (sess?.agent === agent) return av;
      }
    }
    return null;
  })();

  if (cachedModels && cachedModels.length > 0) {
    return (
      <select
        className={input}
        value={value ?? ""}
        disabled={readOnly}
        aria-label="Model"
        onChange={(e) => onChange(e.target.value || null)}
      >
        <option value="">Default model</option>
        {value && !cachedModels.some((m) => m.modelId === value) && (
          <option value={value}>{value}</option>
        )}
        {cachedModels.map((m) => (
          <option key={m.modelId} value={m.modelId}>
            {m.name || m.modelId}
          </option>
        ))}
      </select>
    );
  }

  return (
    <div className="flex flex-col">
      <input
        className={input}
        placeholder="Model (optional)"
        value={value ?? ""}
        readOnly={readOnly}
        onChange={(e) => onChange(e.target.value || null)}
      />
      {!readOnly && (
        <span className="truncate text-[10px] text-muted">
          models load after the agent's first session
        </span>
      )}
    </div>
  );
}

const CAPS: { key: keyof TeamCaps; label: string }[] = [
  { key: "writesItems", label: "Creates and changes items" },
  { key: "dispatches", label: "Starts workers" },
  { key: "decides", label: "Answers advisor decisions" },
  { key: "merges", label: "Records merges" },
];
const MODES = ["default", "auto", "always-approve"];
const btn = "rounded-md border border-border px-2 py-1 text-xs hover:border-accent disabled:opacity-40";
const primary = "rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40";

type RoleDraft = Omit<TeamRole, "id" | "builtin"> & { id?: string };
const blankRole: RoleDraft = { name: "", standing: false, instructions: "", caps: { writesItems: false, dispatches: false, decides: false, merges: false }, reportsTo: null };

/** Roles: built-ins are read-only and copied; your own can be edited and deleted. */
export function RolesEditor() {
  const setToast = useStore((s) => s.setToast);
  const [roles, setRoles] = useState<TeamRole[]>([]);
  const [sel, setSel] = useState<string | "new" | null>(null);
  const [draft, setDraft] = useState<RoleDraft>(blankRole);
  const fail = (e: unknown) => setToast(errorText(e));
  const reload = () => api.listRoles().then(setRoles, fail);
  useEffect(() => {
    reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const current = roles.find((r) => r.id === sel);
  useEffect(() => {
    if (current) setDraft({ id: current.id, name: current.name, standing: current.standing, instructions: current.instructions, caps: current.caps, reportsTo: current.reportsTo });
    else if (sel === "new") setDraft((d) => (d.id ? blankRole : d));
  }, [current, sel]);
  const ro = !!current?.builtin;
  const isDirty = !ro && (
    current
      ? draft.name !== current.name ||
        draft.instructions !== current.instructions ||
        draft.standing !== current.standing ||
        draft.reportsTo !== current.reportsTo ||
        draft.caps.writesItems !== current.caps.writesItems ||
        draft.caps.dispatches !== current.caps.dispatches ||
        draft.caps.decides !== current.caps.decides ||
        draft.caps.merges !== current.caps.merges
      : sel === "new" && (
        draft.name !== "" ||
        draft.instructions !== "" ||
        draft.standing ||
        draft.reportsTo !== null ||
        Object.values(draft.caps).some(Boolean)
      )
  );

  useEffect(() => {
    useStore.getState().setSettingsDirty(isDirty);
    return () => {
      useStore.getState().setSettingsDirty(false);
    };
  }, [isDirty]);

  return (
    <div className="flex gap-4">
      <div className="flex w-44 shrink-0 flex-col gap-0.5">
        {roles.map((r) => (
          <button key={r.id} className={`truncate rounded-md px-2 py-1.5 text-left text-xs ${sel === r.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`} onClick={() => setSel(r.id)}>
            {r.name}
            {r.builtin && <span className="ml-1 text-muted">built-in</span>}
          </button>
        ))}
        <button className="mt-2 rounded-md border border-dashed border-border px-2 py-1.5 text-left text-xs text-muted hover:border-accent hover:text-fg" onClick={() => { setDraft(blankRole); setSel("new"); }}>
          + New role
        </button>
      </div>
      <div className="min-w-0 flex-1">
        {sel && (
          <div className="flex flex-col gap-2 text-xs">
            <input className={input} placeholder="Name, e.g. Careful worker" value={draft.name} readOnly={ro} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
            <textarea className={`${input} h-48 resize-y font-mono text-xs`} placeholder="What this role does and may not do" value={draft.instructions} readOnly={ro} onChange={(e) => setDraft({ ...draft, instructions: e.target.value })} />
            <label className="flex items-center gap-2">
              <input type="checkbox" checked={draft.standing} disabled={ro} onChange={(e) => setDraft({ ...draft, standing: e.target.checked })} />
              Standing seat (stays until retired; otherwise released when its item closes)
            </label>
            {CAPS.map((c) => (
              <label key={c.key} className="flex items-center gap-2">
                <input type="checkbox" checked={draft.caps[c.key]} disabled={ro} onChange={(e) => setDraft({ ...draft, caps: { ...draft.caps, [c.key]: e.target.checked } })} />
                {c.label}
              </label>
            ))}
            <div className="text-muted">Capability changes apply at once to sessions already in this role, on their next tool call.</div>
            <label className="flex items-center gap-2">
              Reports to
              <select className={input} value={draft.reportsTo ?? ""} disabled={ro} onChange={(e) => setDraft({ ...draft, reportsTo: e.target.value || null })}>
                <option value="">Nobody</option>
                {roles.filter((r) => r.id !== draft.id).map((r) => (
                  <option key={r.id} value={r.id}>{r.name}</option>
                ))}
              </select>
            </label>
            <div className="flex gap-2">
              {ro ? (
                <button className={primary} onClick={() => { setDraft({ ...draft, id: undefined, name: `Copy of ${draft.name}` }); setSel("new"); }}>Copy to edit</button>
              ) : (
                <button
                  className={primary}
                  disabled={!draft.name.trim()}
                  onClick={() => api.saveRole(draft).then((r) => { setSel(r.id); window.dispatchEvent(new Event("teams-changed")); return reload(); }, fail)}
                >
                  Save
                </button>
              )}
              {current && !ro && (
                <button className={btn} onClick={() => api.deleteRole(current.id).then(() => { setSel(null); return reload(); }, fail)}>Delete</button>
              )}
            </div>
            <div className="text-muted">A running agent reads a changed role the next time it starts.</div>
          </div>
        )}
      </div>
    </div>
  );
}

type TemplateDraft = { id?: string; name: string; seats: TeamSeat[]; routing: TeamTemplate["routing"] };
const blankSeat = (roleId: string): TeamSeat => ({ seat: "", roleId, agent: "claude-code", model: null, permissionMode: "default", count: 1 });

/** Templates: seats of role, agent and model. The built-in one is copied to change it. */
export function TemplatesEditor() {
  const setToast = useStore((s) => s.setToast);
  const [templates, setTemplates] = useState<TeamTemplate[]>([]);
  const [roles, setRoles] = useState<TeamRole[]>([]);
  const [sel, setSel] = useState<string | "new" | null>(null);
  const [draft, setDraft] = useState<TemplateDraft>({ name: "", seats: [], routing: [] });
  const fail = (e: unknown) => setToast(errorText(e));
  const reload = () => Promise.all([api.listTeamTemplates(), api.listRoles()]).then(([t, r]) => { setTemplates(t); setRoles(r); }, fail);
  useEffect(() => {
    reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const current = templates.find((t) => t.id === sel);
  useEffect(() => {
    if (current) setDraft({ id: current.id, name: current.name, seats: current.seats, routing: current.routing });
  }, [current]);
  const ro = !!current && current.id.startsWith("builtin-");
  const isDirty = !ro && (
    current
      ? draft.name !== current.name || JSON.stringify(draft.seats) !== JSON.stringify(current.seats)
      : sel === "new" && (
        draft.name !== "" ||
        draft.seats.length !== 1 ||
        draft.seats[0]?.seat !== "" ||
        draft.seats[0]?.agent !== "claude-code" ||
        draft.seats[0]?.model !== null ||
        (draft.seats[0]?.choices?.length ?? 0) > 0
      )
  );

  useEffect(() => {
    useStore.getState().setSettingsDirty(isDirty);
    return () => {
      useStore.getState().setSettingsDirty(false);
    };
  }, [isDirty]);

  // Changing what the user acknowledged (agent, model, choices or mode) takes the tick back.
  const setSeat = (i: number, patch: Partial<TeamSeat>) =>
    setDraft({
      ...draft,
      seats: draft.seats.map((s, j) => (j === i ? { ...s, ...("agent" in patch || "model" in patch || "permissionMode" in patch || "choices" in patch ? { allowUnasked: false } : {}), ...patch } : s)),
    });

  return (
    <div className="flex gap-4">
      <div className="flex w-44 shrink-0 flex-col gap-0.5">
        {templates.map((t) => (
          <button key={t.id} className={`truncate rounded-md px-2 py-1.5 text-left text-xs ${sel === t.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`} onClick={() => setSel(t.id)}>
            {t.name}
            {t.id.startsWith("builtin-") && <span className="ml-1 text-muted">built-in</span>}
          </button>
        ))}
        <button className="mt-2 rounded-md border border-dashed border-border px-2 py-1.5 text-left text-xs text-muted hover:border-accent hover:text-fg" onClick={() => { setDraft({ name: "", seats: [blankSeat(roles[0]?.id ?? "")], routing: [] }); setSel("new"); }}>
          + New template
        </button>
      </div>
      <div className="min-w-0 flex-1">
        {sel && (
          <div className="flex flex-col gap-2 text-xs">
            <input className={input} placeholder="Template name" value={draft.name} readOnly={ro} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
            {draft.seats.map((s, i) => {
              const role = roles.find((r) => r.id === s.roleId);
              const isWorker = role && !role.standing;
              const seatTrim = s.seat.trim();
              const seatErr = !seatTrim
                ? "Seat name required"
                : draft.seats.some((o, j) => j !== i && o.seat.trim() === seatTrim)
                  ? "Seat name must be unique"
                  : null;
              const countErr = s.count < 1 ? "Count ≥ 1" : null;
              return (
              <div key={i} className="grid grid-cols-[1fr_1fr_1fr_1fr_auto_auto] items-center gap-1">
                <div className="flex flex-col gap-0.5">
                  <input className={input} placeholder="Seat" value={s.seat} readOnly={ro} onChange={(e) => setSeat(i, { seat: e.target.value })} />
                  {!ro && seatErr && <span className="text-[10px] text-danger">{seatErr}</span>}
                </div>
                <select className={input} value={s.roleId} disabled={ro} onChange={(e) => setSeat(i, { roleId: e.target.value })}>
                  {roles.map((r) => (
                    <option key={r.id} value={r.id}>{r.name}</option>
                  ))}
                </select>
                <select className={input} value={s.agent} disabled={ro} onChange={(e) => setSeat(i, { agent: e.target.value })}>
                  {Object.entries(AGENT_LABELS).map(([id, label]) => (
                    <option key={id} value={id}>{label}</option>
                  ))}
                </select>
                <SeatModelPicker agent={s.agent} value={s.model ?? null} readOnly={ro} onChange={(m) => setSeat(i, { model: m })} />
                <label className="flex items-center gap-1 whitespace-nowrap text-muted">
                  <span>{isWorker ? "Max workers" : "Sessions"}</span>
                  <div className="flex flex-col gap-0.5">
                    <input
                      className={`${input} w-14`}
                      type="number"
                      min={1}
                      value={s.count}
                      readOnly={ro}
                      aria-label={isWorker ? "Max workers" : "Sessions"}
                      onChange={(e) => setSeat(i, { count: Math.floor(Number(e.target.value)) })}
                    />
                    {!ro && countErr && <span className="text-[10px] text-danger">{countErr}</span>}
                  </div>
                </label>
                <select className={input} value={s.permissionMode} disabled={ro} onChange={(e) => setSeat(i, { permissionMode: e.target.value })}>
                  {MODES.map((m) => (
                    <option key={m} value={m}>{m}</option>
                  ))}
                </select>
                {isWorker && (
                  <div className="col-span-6 flex flex-col gap-1 rounded border border-border/60 p-2">
                    <div className="text-muted">Agent and model choices the coordinator may pick from:</div>
                    {(s.choices ?? []).map((c, ci) => (
                      <div key={ci} className="flex items-center gap-1">
                        <select
                          className={input}
                          value={c.agent}
                          disabled={ro}
                          onChange={(e) => {
                            const cur = s.choices ?? [];
                            setSeat(i, { choices: cur.map((x, cj) => (cj === ci ? { ...x, agent: e.target.value } : x)) });
                          }}
                        >
                          {Object.entries(AGENT_LABELS).map(([id, label]) => (
                            <option key={id} value={id}>{label}</option>
                          ))}
                        </select>
                        <SeatModelPicker
                          agent={c.agent}
                          value={c.model ?? null}
                          readOnly={ro}
                          onChange={(m) => {
                            const cur = s.choices ?? [];
                            setSeat(i, { choices: cur.map((x, cj) => (cj === ci ? { ...x, model: m } : x)) });
                          }}
                        />
                        {!ro && (
                          <button
                            className="px-1 text-muted hover:text-fg"
                            onClick={() => {
                              const cur = s.choices ?? [];
                              setSeat(i, { choices: cur.filter((_, cj) => cj !== ci) });
                            }}
                          >
                            Remove
                          </button>
                        )}
                      </div>
                    ))}
                    {!ro && (
                      <button
                        className={`${btn} self-start`}
                        onClick={() => {
                          const cur = s.choices ?? [];
                          setSeat(i, { choices: [...cur, { agent: "claude-code", model: null }] });
                        }}
                      >
                        + Add choice
                      </button>
                    )}
                  </div>
                )}
                {(s.gaps?.length ?? 0) > 0 && (
                  <label className="col-span-6 flex items-start gap-1.5 rounded border border-border p-1.5">
                    <input type="checkbox" className="mt-0.5" checked={!!s.allowUnasked} disabled={ro} onChange={(e) => setSeat(i, { allowUnasked: e.target.checked })} />
                    <span>
                      Let dispatched workers use agents that can act without asking. Without this tick the coordinator can't start them. It covers this seat's agent, model, choices and mode as they are now.
                      {s.gaps!.map((g) => (
                        <span key={g.agent} className="block text-muted">{(AGENT_LABELS as Record<string, string>)[g.agent] ?? g.agent}: {g.gaps.join(" ")}</span>
                      ))}
                    </span>
                  </label>
                )}
                {(s.prompts?.length ?? 0) > 0 && (
                  <div className="col-span-6 text-muted">
                    {s.prompts!.map((a) => (AGENT_LABELS as Record<string, string>)[a] ?? a).join(", ")}: team tools still wait for your approval on every call (no automatic allow for this agent), so it can't run unattended.
                  </div>
                )}
                {!ro && <button className="col-span-6 justify-self-end text-muted hover:text-fg" onClick={() => setDraft({ ...draft, seats: draft.seats.filter((_, j) => j !== i) })}>Remove seat</button>}
              </div>
            );
            })}
            {!ro && <button className={`${btn} self-start`} onClick={() => setDraft({ ...draft, seats: [...draft.seats, blankSeat(roles[0]?.id ?? "")] })}>+ Add seat</button>}
            <div className="text-muted">Seat count: members of a standing seat, or the most workers alive at once.</div>
            <div className="flex gap-2">
              {ro ? (
                <button className={primary} onClick={() => { setDraft({ ...draft, id: undefined, name: `Copy of ${draft.name}` }); setSel("new"); }}>Copy to edit</button>
              ) : (
                <button
                  className={primary}
                  disabled={
                    !draft.name.trim() ||
                    draft.seats.length === 0 ||
                    draft.seats.some((s, i) => {
                      const seatTrim = s.seat.trim();
                      return !seatTrim || draft.seats.some((o, j) => j !== i && o.seat.trim() === seatTrim) || s.count < 1;
                    })
                  }
                  onClick={() => api.saveTemplate(draft).then((t) => { setSel(t.id); return reload(); }, fail)}
                >
                  Save
                </button>
              )}
              {current && !ro && (
                <button className={btn} onClick={() => api.deleteTemplate(current.id).then(() => { setSel(null); return reload(); }, fail)}>Delete</button>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
