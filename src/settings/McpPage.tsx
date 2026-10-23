import { useEffect, useState } from "react";
import { api, errorText, type KeyValue, type McpConfig, type McpServer } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";
import { Toggle } from "../components/Toggle";
import { Dot } from "../components/Dot";

const blank = (): McpConfig => ({ transport: "stdio", command: "", args: [], env: [], url: "", headers: [] });

function Pairs({
  label,
  rows,
  onChange,
}: {
  label: string;
  rows: KeyValue[];
  onChange: (rows: KeyValue[]) => void;
}) {
  const set = (i: number, patch: Partial<KeyValue>) => onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <div className="flex flex-col gap-1">
      <div className="text-xs text-muted">{label}</div>
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-2">
          <input
            className={`${input} w-44 font-mono text-xs`}
            placeholder="NAME"
            value={r.name}
            onChange={(e) => set(i, { name: e.target.value })}
          />
          <input
            className={`${input} flex-1 font-mono text-xs`}
            type={r.secret ? "password" : "text"}
            placeholder={r.secret && r.stored ? "•••••• (stored; leave blank to keep)" : "value"}
            value={r.value}
            onChange={(e) => set(i, { value: e.target.value })}
          />
          <label className="flex items-center gap-1 text-[12px] text-muted" title="Keep the value in the OS keyring">
            <input type="checkbox" checked={r.secret} onChange={(e) => set(i, { secret: e.target.checked })} />
            secret
          </label>
          <button className="text-muted hover:text-danger" onClick={() => onChange(rows.filter((_, j) => j !== i))} aria-label="Remove">
            ×
          </button>
        </div>
      ))}
      <button
        className="self-start text-xs text-accent-ink hover:underline"
        onClick={() => onChange([...rows, { name: "", value: "", secret: false }])}
      >
        + Add
      </button>
    </div>
  );
}

/** MCP servers handed to every Code session through ACP (`mcpServers`). */
export function McpPage() {
  const setToast = useStore((s) => s.setToast);
  const askConfirm = useStore((s) => s.askConfirm);
  const bump = useStore((s) => s.bumpAgentConfig);
  const [servers, setServers] = useState<McpServer[]>([]);
  const [sel, setSel] = useState<string | "new" | null>(null);
  const [name, setName] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [cfg, setCfg] = useState<McpConfig>(blank());
  const [argsText, setArgsText] = useState("");

  const reload = async (select?: string) => {
    setServers(await api.listMcpServers());
    if (select) setSel(select);
  };
  useEffect(() => {
    reload().catch((e) => setToast(errorText(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => {
    const s = servers.find((x) => x.id === sel);
    setName(s?.name ?? "");
    setEnabled(s?.enabled ?? true);
    const c = s ? { ...blank(), ...s.config } : blank();
    setCfg(c);
    setArgsText(c.args.join("\n"));
  }, [sel, servers]);

  const current = servers.find((x) => x.id === sel);

  const save = async () => {
    try {
      const args = argsText.split("\n").map((a) => a.trim()).filter(Boolean);
      const s = await api.saveMcpServer({ id: current?.id, name, enabled, config: { ...cfg, args } });
      await reload(s.id);
      bump();
      setToast("Saved. Idle Code sessions restart to pick it up.");
    } catch (e) {
      setToast(errorText(e));
    }
  };

  return (
    <div className="flex max-w-3xl gap-4 pb-4">
      <div className="flex w-48 shrink-0 flex-col gap-0.5">
        {servers.map((s) => (
          <button
            key={s.id}
            className={`flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs ${sel === s.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
            onClick={() => setSel(s.id)}
          >
            <Dot size={6} hollow={s.enabled ? undefined : 1} className={s.enabled ? "text-ok" : "text-muted"} />
            <span className="min-w-0 flex-1 truncate">{s.name}</span>
            <span className="text-[11px] text-muted">{s.config.transport}</span>
          </button>
        ))}
        {servers.length === 0 && <div className="px-2 py-1.5 text-xs text-muted">No servers</div>}
        <button
          className="mt-2 rounded-md border border-dashed border-border px-2 py-1.5 text-left text-xs text-muted hover:border-accent hover:text-fg"
          onClick={() => setSel("new")}
        >
          + Add server
        </button>
      </div>
      <div className="min-w-0 flex-1">
        {!sel ? (
          <p className="text-xs text-muted">
            MCP servers give the Code agent extra tools. Patchbay passes them to every session through the Agent Client
            Protocol, so they work with any ACP agent. Stdio servers always work; HTTP and SSE need agent support.
          </p>
        ) : (
          <div className="flex flex-col gap-3">
            <div className="flex items-center gap-3">
              <input className={`${input} flex-1`} placeholder="Name, e.g. github" value={name} onChange={(e) => setName(e.target.value)} />
              <label className="flex items-center gap-2 text-xs text-muted">
                Enabled <Toggle label="Enabled" checked={enabled} onChange={setEnabled} />
              </label>
            </div>
            <div className="flex rounded-md border border-border bg-panel p-0.5 text-xs" role="radiogroup">
              {(["stdio", "http", "sse"] as const).map((t) => (
                <button
                  key={t}
                  role="radio"
                  aria-checked={cfg.transport === t}
                  className={`flex-1 rounded px-2.5 py-1 ${cfg.transport === t ? "bg-bg text-fg shadow-sm" : "text-muted hover:text-fg"}`}
                  onClick={() => setCfg({ ...cfg, transport: t })}
                >
                  {t === "stdio" ? "Local command (stdio)" : t.toUpperCase()}
                </button>
              ))}
            </div>
            {cfg.transport === "stdio" ? (
              <>
                <input
                  className={`${input} font-mono text-xs`}
                  placeholder="Command, e.g. npx"
                  value={cfg.command}
                  onChange={(e) => setCfg({ ...cfg, command: e.target.value })}
                />
                <textarea
                  className={`${input} h-20 resize-y font-mono text-xs`}
                  placeholder={"Arguments, one per line\n-y\n@modelcontextprotocol/server-github"}
                  value={argsText}
                  onChange={(e) => setArgsText(e.target.value)}
                />
                <Pairs label="Environment variables" rows={cfg.env} onChange={(env) => setCfg({ ...cfg, env })} />
              </>
            ) : (
              <>
                <input
                  className={`${input} font-mono text-xs`}
                  placeholder="https://example.com/mcp"
                  value={cfg.url}
                  onChange={(e) => setCfg({ ...cfg, url: e.target.value })}
                />
                <Pairs label="Headers" rows={cfg.headers} onChange={(headers) => setCfg({ ...cfg, headers })} />
              </>
            )}
            <div className="flex items-center gap-3">
              <button className="rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40" disabled={!name.trim()} onClick={save}>
                Save
              </button>
              {current && (
                <button
                  className="text-xs text-muted hover:text-danger"
                  onClick={async () => {
                    const { ok } = await askConfirm({
                      title: "Remove MCP server?",
                      body: `"${current.name}" and its stored secrets will be removed.`,
                      confirmLabel: "Remove",
                      danger: true,
                    });
                    if (!ok) return;
                    await api.deleteMcpServer(current.id);
                    setSel(null);
                    await reload();
                    bump();
                  }}
                >
                  Remove
                </button>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
