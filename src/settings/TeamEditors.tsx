import { useEffect, useState } from "react";
import { AGENT_LABELS, api, errorText, type TeamCaps, type TeamRole, type TeamSeat, type TeamTemplate } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";

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
  const setSeat = (i: number, patch: Partial<TeamSeat>) => setDraft({ ...draft, seats: draft.seats.map((s, j) => (j === i ? { ...s, ...patch } : s)) });

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
            {draft.seats.map((s, i) => (
              <div key={i} className="grid grid-cols-[1fr_1fr_1fr_1fr_4rem_auto] gap-1">
                <input className={input} placeholder="Seat" value={s.seat} readOnly={ro} onChange={(e) => setSeat(i, { seat: e.target.value })} />
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
                <input className={input} placeholder="Model (optional)" value={s.model ?? ""} readOnly={ro} onChange={(e) => setSeat(i, { model: e.target.value || null })} />
                <input className={input} type="number" min={1} value={s.count} readOnly={ro} onChange={(e) => setSeat(i, { count: Math.max(1, Number(e.target.value) || 1) })} />
                <select className={input} value={s.permissionMode} disabled={ro} onChange={(e) => setSeat(i, { permissionMode: e.target.value })}>
                  {MODES.map((m) => (
                    <option key={m} value={m}>{m}</option>
                  ))}
                </select>
                {!ro && <button className="col-span-6 justify-self-end text-muted hover:text-fg" onClick={() => setDraft({ ...draft, seats: draft.seats.filter((_, j) => j !== i) })}>Remove seat</button>}
              </div>
            ))}
            {!ro && <button className={`${btn} self-start`} onClick={() => setDraft({ ...draft, seats: [...draft.seats, blankSeat(roles[0]?.id ?? "")] })}>+ Add seat</button>}
            <div className="text-muted">Seat count: members of a standing seat, or the most workers alive at once.</div>
            <div className="flex gap-2">
              {ro ? (
                <button className={primary} onClick={() => { setDraft({ ...draft, id: undefined, name: `Copy of ${draft.name}` }); setSel("new"); }}>Copy to edit</button>
              ) : (
                <button className={primary} disabled={!draft.name.trim() || draft.seats.length === 0} onClick={() => api.saveTemplate(draft).then((t) => { setSel(t.id); return reload(); }, fail)}>Save</button>
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
