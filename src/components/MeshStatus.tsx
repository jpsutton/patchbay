import { useEffect, useRef } from "react";
import type { MeshMember, MeshStatus } from "../lib/ipc";
import { useStore } from "../lib/store";
import { MeshIcon, TriangleAlertIcon, XIcon } from "./icons";

/** The other computers in the mesh, and how many of them are connected. */
function summary(mesh: MeshStatus) {
  const others = mesh.members.filter((m) => m.state !== "self" && m.state !== "removed");
  const online = others.filter((m) => m.state === "direct" || m.state === "relay").length;
  return { others, online };
}

/** Red for a problem, amber while someone asks to join, green with a computer connected, grey alone. */
function dotClass(mesh: MeshStatus) {
  const { online } = summary(mesh);
  if (mesh.problem) return "bg-danger";
  if (mesh.pending.length > 0) return "signal-pulse bg-accent";
  return online > 0 ? "bg-ok" : "bg-faint";
}

function title(mesh: MeshStatus) {
  const { others, online } = summary(mesh);
  if (mesh.problem) return `Mesh error: ${mesh.problem.title}`;
  if (others.length === 0) return "Mesh: no other computers yet";
  return `Mesh: ${online} of ${others.length} computers connected`;
}

function memberDot(m: MeshMember) {
  if (m.authFailed) return "bg-danger";
  if (m.state === "direct") return "bg-ok";
  if (m.state === "relay") return "bg-live";
  return "bg-faint opacity-60";
}

function memberStatus(m: MeshMember) {
  if (m.authFailed) return "auth failed";
  if (m.state === "direct") return m.rttMs != null ? `direct · ${m.rttMs} ms` : "direct";
  if (m.state === "relay") return `via ${m.viaName || "another computer"}`;
  return "offline";
}

/** Only for computers in a mesh: everyone else has nothing to watch. */
function useMesh() {
  const mesh = useStore((s) => s.mesh);
  return mesh?.member ? mesh : null;
}

/** Status chip in the sidebar footer; opens the mesh popover. */
export function MeshChip() {
  const mesh = useMesh();
  const open = useStore((s) => s.meshPopoverOpen);
  const setOpen = useStore((s) => s.setMeshPopoverOpen);
  if (!mesh) return null;
  const { others, online } = summary(mesh);
  const err = !!mesh.problem;
  return (
    <button
      data-mesh-toggle
      className={`flex h-9 shrink-0 items-center gap-[7px] whitespace-nowrap rounded-lg px-2.5 text-[13px] ${
        err
          ? "bg-danger/10 text-danger hover:bg-danger/15"
          : `text-muted hover:bg-panel-2 hover:text-fg ${open ? "bg-panel-2 text-fg" : ""}`
      }`}
      title={title(mesh)}
      aria-label={title(mesh)}
      aria-expanded={open}
      onClick={() => setOpen(!open)}
    >
      <span className={`h-2 w-2 shrink-0 rounded-full ${dotClass(mesh)}`} aria-hidden="true" />
      {err ? (
        // The word drops out in a narrow sidebar footer; the dot (red on error) and count stay.
        <span className="@max-[320px]/footer:hidden">Mesh error</span>
      ) : (
        <>
          <span className="@max-[320px]/footer:hidden">Mesh</span>
          {others.length > 0 && (
            <span className="meta text-faint">
              {online}/{others.length}
            </span>
          )}
        </>
      )}
    </button>
  );
}

/** The collapsed sidebar's mesh light. */
export function MeshRailButton() {
  const mesh = useMesh();
  const open = useStore((s) => s.meshPopoverOpen);
  const setOpen = useStore((s) => s.setMeshPopoverOpen);
  if (!mesh) return null;
  return (
    <button
      data-mesh-toggle
      className="relative flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg"
      title={title(mesh)}
      aria-label={title(mesh)}
      aria-expanded={open}
      onClick={() => setOpen(!open)}
    >
      <MeshIcon />
      <span
        className={`absolute bottom-1 right-1 h-[7px] w-[7px] rounded-full ring-2 ring-panel ${dotClass(mesh)}`}
        aria-hidden="true"
      />
    </button>
  );
}

/** Who's in the mesh and what's wrong with it, above the sidebar footer. */
export function MeshPopover({ className }: { className: string }) {
  const mesh = useMesh();
  const open = useStore((s) => s.meshPopoverOpen);
  const setOpen = useStore((s) => s.setMeshPopoverOpen);
  const openSettingsAt = useStore((s) => s.openSettingsAt);
  const box = useRef<HTMLDivElement>(null);
  const shown = open && !!mesh;

  useEffect(() => {
    if (!shown) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element;
      // The toggle closes it itself; closing here too would reopen it.
      if (!box.current?.contains(t) && !t.closest?.("[data-mesh-toggle]")) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
      }
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [shown, setOpen]);

  if (!shown) return null;
  const { others } = summary(mesh);
  const p = mesh.problem;
  return (
    <div
      ref={box}
      role="dialog"
      aria-label="Mesh status"
      className={`z-40 w-[280px] rounded-xl border border-border bg-raised py-1.5 text-[13px] shadow-float ${className}`}
    >
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Mesh</span>
        <span className="text-[11px] text-faint">this computer: {mesh.name}</span>
      </div>
      {p && (
        <div className="mx-2 mb-1.5 mt-1 rounded-lg bg-danger/10 px-2.5 py-2 text-[12.5px] leading-[1.4] text-danger" role="alert">
          <b className="font-semibold">{p.title}.</b> {p.message}
        </div>
      )}
      {others.map((m) => (
        <div key={m.nodeId} className="flex items-center gap-[9px] px-3 py-1.5">
          <span className={`h-2 w-2 shrink-0 rounded-full ${memberDot(m)}`} aria-hidden="true" />
          <span className={`min-w-0 flex-1 truncate ${m.state === "offline" && !m.authFailed ? "text-muted" : ""}`}>
            {m.name}
            {m.role === "remote" && <span className="meta text-faint"> phone</span>}
          </span>
          <span className={`meta shrink-0 ${m.authFailed ? "text-danger" : "text-faint"}`}>{memberStatus(m)}</span>
        </div>
      ))}
      {others.length === 0 && <p className="px-3 py-1.5 text-muted">No other computers yet.</p>}
      {mesh.pending.length > 0 && (
        <p className="px-3 py-1.5 text-accent-ink">
          {mesh.pending.map((x) => x.name).join(", ")} {mesh.pending.length === 1 ? "asks" : "ask"} to join.
        </p>
      )}
      <div className="my-1.5 border-t border-line" />
      <button
        className="mx-1 flex w-[calc(100%-0.5rem)] rounded-md px-2 py-1.5 text-left text-muted hover:bg-panel-2 hover:text-fg"
        onClick={() => {
          setOpen(false);
          openSettingsAt("mesh");
        }}
      >
        Mesh settings…
      </button>
    </div>
  );
}

/** A problem toast: red edge, stays until dismissed or the problem is fixed. */
export function AlertToast() {
  const alert = useStore((s) => s.alert);
  const setAlert = useStore((s) => s.setAlert);
  if (!alert) return null;
  return (
    <div
      role="alert"
      className="flex w-[380px] gap-3 rounded-xl border border-l-[3px] border-border border-l-danger bg-raised px-3.5 py-3 text-[13px] shadow-float"
    >
      <span className="mt-px shrink-0 text-danger">
        <TriangleAlertIcon />
      </span>
      <div className="min-w-0 flex-1">
        <b className="mb-0.5 block font-semibold">{alert.title}</b>
        <p className="leading-[1.45] text-muted">{alert.body}</p>
        {alert.actions.length > 0 && (
          <div className="mt-2.5 flex gap-2">
            {alert.actions.map((a) => (
              <button
                key={a.label}
                className={`h-7 rounded-[7px] border px-2.5 text-[12.5px] ${
                  a.primary
                    ? "border-accent bg-accent font-medium text-accent-fg hover:brightness-105"
                    : "border-border bg-panel text-fg hover:bg-panel-2"
                }`}
                onClick={() => {
                  setAlert(null);
                  a.run();
                }}
              >
                {a.label}
              </button>
            ))}
          </div>
        )}
      </div>
      <button className="shrink-0 self-start text-faint hover:text-fg" aria-label="Dismiss" onClick={() => setAlert(null)}>
        <XIcon size={14} />
      </button>
    </div>
  );
}
