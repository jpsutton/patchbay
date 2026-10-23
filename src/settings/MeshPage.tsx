import { useEffect, useState } from "react";
import { api, errorText, type MeshMember } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";
import { Deferred, Row } from "./PreferencesPage";
import { Dot } from "../components/Dot";

const primary = "rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40";
const secondary = "rounded-md border border-border px-3 py-1.5 text-xs hover:bg-panel-2 disabled:opacity-40";

function Code({ code }: { code: string }) {
  return <span className="font-mono text-[22px] font-medium tracking-[0.12em] text-fg">{code}</span>;
}

const STATE_LABEL: Record<MeshMember["state"], string> = {
  self: "This computer",
  direct: "Connected",
  relay: "Connected",
  offline: "Offline",
  removed: "Removed",
};

const STATE_DOT: Record<MeshMember["state"], string> = {
  self: "text-accent",
  direct: "text-ok",
  relay: "text-live",
  offline: "text-faint",
  removed: "text-danger",
};

function MemberRow({ m, onRemove }: { m: MeshMember; onRemove?: () => void }) {
  const phone = m.role === "remote";
  const label = m.authFailed ? "Passphrase doesn't match" : phone && m.state === "relay" ? "Relayed" : STATE_LABEL[m.state];
  const detail =
    m.state === "relay"
      ? `through ${m.viaName || "another computer"}`
      : m.state === "direct" && m.rttMs != null
        ? `${m.rttMs} ms`
        : null;
  return (
    <div className="flex items-center gap-3 border-b border-border py-2.5 last:border-b-0">
      <Dot className={m.authFailed ? "text-danger" : STATE_DOT[m.state]} />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm">{m.name}</div>
        <div className="text-[12px] text-muted">
          {phone && <span className="text-faint">Phone · </span>}
          {label}
          {detail && <span className="text-faint"> · {detail}</span>}
        </div>
      </div>
      {onRemove && m.state !== "removed" && (
        <button className="text-xs text-muted hover:text-danger" onClick={onRemove}>
          Remove
        </button>
      )}
    </div>
  );
}

function Setup() {
  const mesh = useStore((s) => s.mesh);
  const refreshMesh = useStore((s) => s.refreshMesh);
  const [name, setName] = useState(mesh?.name ?? "");
  const [pass, setPass] = useState("");
  const [pass2, setPass2] = useState("");
  const [url, setUrl] = useState("");
  const [joinPass, setJoinPass] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => setName((n) => n || mesh?.name || ""), [mesh?.name]);

  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await refreshMesh();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex max-w-2xl flex-col gap-4 pb-4">
      <p className="text-xs leading-5 text-muted">
        Link your computers into a mesh. Chats, Code sessions, prompts, providers and MCP servers (API keys included,
        encrypted) sync between them, live. From any of them you can drive a Code session running on another, or move the session over.
        Computers connect directly over a network you already have, such as Tailscale, ZeroTier, a VPN or a Cloudflare
        Tunnel. Nothing goes through a cloud service.
      </p>
      <Row label="This computer's name" hint="How other computers in the mesh show it.">
        <input className={`${input} w-56`} value={name} onChange={(e) => setName(e.target.value)} />
      </Row>
      <div className="grid grid-cols-2 gap-3">
        <section className="flex flex-col gap-2 rounded-xl border border-border p-4" aria-label="Start a mesh">
          <div className="text-sm font-medium">Start a mesh</div>
          <p className="text-[12px] leading-5 text-muted">
            Pick a passphrase. Other computers need it to ask to join, and you approve each one here.
          </p>
          <input
            className={input}
            type="password"
            placeholder="Passphrase (8+ characters)"
            value={pass}
            onChange={(e) => setPass(e.target.value)}
          />
          <input
            className={input}
            type="password"
            placeholder="Passphrase again"
            value={pass2}
            onChange={(e) => setPass2(e.target.value)}
          />
          <button
            className={`${primary} self-start`}
            disabled={busy || pass.length < 8 || pass !== pass2}
            onClick={() => run(() => api.meshCreate(pass, name))}
          >
            Start mesh
          </button>
        </section>
        <section className="flex flex-col gap-2 rounded-xl border border-border p-4" aria-label="Join a mesh">
          <div className="text-sm font-medium">Join a mesh</div>
          <p className="text-[12px] leading-5 text-muted">
            Enter the address of a computer already in the mesh, such as <span className="font-mono">100.64.0.7</span>{" "}
            or <span className="font-mono">wss://mesh.example.com</span>.
          </p>
          <input className={input} placeholder="Address" value={url} onChange={(e) => setUrl(e.target.value)} />
          <input
            className={input}
            type="password"
            placeholder="The mesh's passphrase"
            value={joinPass}
            onChange={(e) => setJoinPass(e.target.value)}
          />
          <button
            className={`${primary} self-start`}
            disabled={busy || !url.trim() || joinPass.length < 8}
            onClick={() => run(() => api.meshJoin(url, joinPass, name))}
          >
            Ask to join
          </button>
        </section>
      </div>
      {error && (
        <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger" role="alert">
          {error}
        </div>
      )}
    </div>
  );
}

function Joining() {
  const mesh = useStore((s) => s.mesh)!;
  const j = mesh.joining!;
  const refreshMesh = useStore((s) => s.refreshMesh);
  return (
    <div className="flex max-w-xl flex-col gap-4 pb-4">
      {j.status === "waiting" && j.code ? (
        <section className="flex flex-col items-start gap-3 rounded-xl border border-accent/50 bg-accent/5 p-4">
          <div className="text-sm font-medium">Waiting for approval</div>
          <p className="text-xs leading-5 text-muted">
            On a computer already in the mesh, open Settings → Mesh and approve <b className="text-fg">{mesh.name}</b>.
            Check that it shows this code:
          </p>
          <Code code={j.code} />
          <p className="text-[12px] text-faint">If the codes differ, decline there: something is between the two computers.</p>
        </section>
      ) : j.status === "rejected" ? (
        <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
          The request was declined{j.error ? `: ${j.error}` : "."}
        </div>
      ) : j.status === "failed" ? (
        <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
          {j.error ?? "Couldn't join."}
        </div>
      ) : (
        <div className="flex items-center gap-2 text-xs text-muted">
          <Dot size={6} className="signal-pulse text-accent" />
          Connecting to <span className="font-mono">{j.url}</span>…
          {j.error && <span className="text-danger">({j.error}; retrying)</span>}
        </div>
      )}
      <button
        className={`${secondary} self-start`}
        onClick={async () => {
          await api.meshCancelJoin();
          refreshMesh();
        }}
      >
        {j.status === "failed" || j.status === "rejected" ? "Back" : "Cancel"}
      </button>
    </div>
  );
}

/** What's wrong, and for a passphrase problem, a place to enter it again. */
function ProblemBox() {
  const mesh = useStore((s) => s.mesh)!;
  const refreshMesh = useStore((s) => s.refreshMesh);
  const [pass, setPass] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const p = mesh.problem;
  if (!p) return null;
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.meshSetPassphrase(pass);
      setPass("");
      await refreshMesh();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="flex flex-col gap-3 rounded-xl border border-danger/40 bg-danger/5 p-4" role="alert">
      <div>
        <div className="text-sm font-medium text-danger">{p.title}</div>
        <p className="mt-0.5 text-[12px] leading-5 text-muted">{p.message}</p>
      </div>
      {(p.kind === "auth" || p.kind === "passphrase") && (
        <div className="flex gap-2">
          <input
            className={`${input} min-w-0 flex-1`}
            type="password"
            placeholder="The mesh's passphrase"
            value={pass}
            onChange={(e) => setPass(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && pass.length >= 8 && !busy && save()}
          />
          <button className={`${primary} shrink-0 whitespace-nowrap`} disabled={busy || pass.length < 8} onClick={save}>
            Save passphrase
          </button>
        </div>
      )}
      {error && <div className="text-xs text-danger">{error}</div>}
    </section>
  );
}

function Member() {
  const mesh = useStore((s) => s.mesh)!;
  const refreshMesh = useStore((s) => s.refreshMesh);
  const askConfirm = useStore((s) => s.askConfirm);
  const setToast = useStore((s) => s.setToast);
  const [seed, setSeed] = useState("");
  const others = mesh.members.filter((m) => m.state !== "self" && m.state !== "removed");
  const removed = mesh.members.filter((m) => m.state === "removed");
  const me = mesh.members.find((m) => m.state === "self");

  const act = async (f: () => Promise<void>) => {
    try {
      await f();
    } catch (e) {
      setToast(errorText(e));
    }
    refreshMesh();
  };

  return (
    <div className="flex max-w-2xl flex-col gap-5 pb-4">
      <ProblemBox />
      {mesh.pending.map((p) => (
        <section
          key={p.nodeId}
          className="flex items-center gap-4 rounded-xl border border-accent/50 bg-accent/5 p-4"
          aria-label={`Join request from ${p.name}`}
        >
          <div className="min-w-0 flex-1">
            <div className="text-sm font-medium">
              {p.name} asks to join{p.role === "remote" ? " as a phone" : ""}
            </div>
            <div className="mt-0.5 text-[12px] leading-5 text-muted">
              {p.role === "remote"
                ? `Approve only if ${p.name} shows the same code. It can read every chat and session and run commands through their agents, but gets no API keys.`
                : `Approve only if ${p.name} shows the same code. It will get all chats, sessions and API keys.`}
              {p.host && p.host !== mesh.nodeId && p.hostName && (
                <span className="text-faint"> It is connected to {p.hostName}; approving here works too.</span>
              )}
            </div>
          </div>
          <Code code={p.code} />
          <div className="flex shrink-0 gap-2">
            <button className={secondary} onClick={() => act(() => api.meshReject(p.nodeId))}>
              Decline
            </button>
            <button className={primary} onClick={() => act(() => api.meshApprove(p.nodeId))}>
              Approve
            </button>
          </div>
        </section>
      ))}

      <section aria-label="Computers">
        <div className="mb-1 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">Computers</div>
        {me && <MemberRow m={me} />}
        {others.map((m) => (
          <MemberRow
            key={m.nodeId}
            m={m}
            onRemove={async () => {
              const { ok } = await askConfirm({
                title: `Remove ${m.name}?`,
                body:
                  m.role === "remote"
                    ? `${m.name} is cut off from the mesh. It never held API keys or a copy of your data.`
                    : `${m.name} is cut off from the mesh and gets no further changes. What it already synced stays on it, ` +
                      "including API keys, so consider replacing those keys with your providers.",
                confirmLabel: "Remove",
                danger: true,
              });
              if (ok) act(() => api.meshRemove(m.nodeId));
            }}
          />
        ))}
        {others.length === 0 && (
          <p className="py-2 text-xs text-muted">
            No other computers yet. On another computer, open Settings → Mesh, choose Join a mesh and enter this
            computer's address{mesh.port ? ` (port ${mesh.port})` : ""}.
          </p>
        )}
        {removed.length > 0 && (
          <p className="pt-2 text-[12px] text-faint">Removed: {removed.map((m) => m.name).join(", ")}</p>
        )}
      </section>

      <section aria-label="This computer">
        <div className="mb-1 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">This computer</div>
        <Row label="Name">
          <Deferred
            value={mesh.name}
            onCommit={async (v) => {
              try {
                await api.meshSetConfig({ name: v });
                await refreshMesh();
                return null;
              } catch (e) {
                return errorText(e);
              }
            }}
          />
        </Row>
        <Row
          label="Listen on"
          hint={
            mesh.running && mesh.port ? (
              <span>Listening on port {mesh.port}.</span>
            ) : (
              "Address and port other computers connect to. Changes restart the connection."
            )
          }
        >
          <Deferred
            mono
            value={mesh.listen}
            placeholder="0.0.0.0:41890"
            onCommit={async (v) => {
              try {
                await api.meshSetConfig({ listen: v });
                await refreshMesh();
                return null;
              } catch (e) {
                return errorText(e);
              }
            }}
          />
        </Row>
        <Row
          label="Public addresses"
          hint="Extra addresses others can reach this computer at, one per line, such as a Cloudflare Tunnel hostname (wss://mesh.example.com). Its network address is found on its own."
        >
          <textarea
            className={`${input} h-20 w-56 resize-y font-mono text-xs`}
            defaultValue={mesh.advertise.join("\n")}
            placeholder="wss://mesh.example.com"
            onBlur={(e) =>
              act(() =>
                api.meshSetConfig({
                  advertise: e.target.value
                    .split("\n")
                    .map((x) => x.trim())
                    .filter(Boolean),
                }),
              )
            }
          />
        </Row>
        <Row label="Add an address" hint="Another computer's address, to find the mesh again if every known address changed.">
          <div className="flex gap-2">
            <input
              className={`${input} w-44 font-mono text-xs`}
              placeholder="100.64.0.7"
              value={seed}
              onChange={(e) => setSeed(e.target.value)}
            />
            <button
              className={secondary}
              disabled={!seed.trim()}
              onClick={() =>
                act(async () => {
                  await api.meshAddAddress(seed);
                  setSeed("");
                })
              }
            >
              Add
            </button>
          </div>
        </Row>
      </section>

      <button
        className="self-start text-xs text-muted hover:text-danger"
        onClick={async () => {
          const { ok } = await askConfirm({
            title: "Leave the mesh?",
            body: "This computer tells the connected members it's leaving, then stops syncing and forgets the passphrase. Its chats and sessions stay here.",
            confirmLabel: "Leave",
            danger: true,
          });
          if (ok) act(() => api.meshLeave());
        }}
      >
        Leave mesh
      </button>
    </div>
  );
}

/** Settings → Mesh: set up, join, approve and manage computers. */
export function MeshPage() {
  const mesh = useStore((s) => s.mesh);
  const refreshMesh = useStore((s) => s.refreshMesh);
  useEffect(() => {
    refreshMesh();
    // Keep connection states and latencies current while the page is open.
    const t = setInterval(refreshMesh, 3000);
    return () => clearInterval(t);
  }, [refreshMesh]);
  if (!mesh) return <p className="text-xs text-muted">Loading…</p>;
  if (mesh.member) return <Member />;
  if (mesh.joining) return <Joining />;
  return <Setup />;
}
