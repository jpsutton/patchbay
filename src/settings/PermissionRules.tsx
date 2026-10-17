import { useEffect, useState } from "react";
import { api, errorText, type PermissionRule } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";

const KINDS = ["edit", "execute", "read", "delete", "move", "search", "fetch"];

/** Saved "always allow" rules; matching happens in the backend for any ACP agent. */
export function PermissionRulesList() {
  const setToast = useStore((s) => s.setToast);
  const sessions = useStore((s) => s.codeSessions);
  const [rules, setRules] = useState<PermissionRule[]>([]);
  const [draft, setDraft] = useState({ cwd: "", toolKind: "execute", pattern: "" });
  const folders = [...new Set(sessions.map((s) => s.cwd))].sort();

  const reload = () => api.listPermissionRules().then(setRules).catch((e) => setToast(errorText(e)));
  useEffect(() => {
    reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const add = async () => {
    try {
      await api.addPermissionRule(draft.cwd || "*", draft.toolKind, draft.toolKind === "execute" ? draft.pattern : "");
      setDraft({ ...draft, pattern: "" });
      reload();
    } catch (e) {
      setToast(errorText(e));
    }
  };

  return (
    <div className="mt-6 border-t border-border pt-4">
      <div className="text-sm">Always-allow rules</div>
      <div className="mt-0.5 text-xs text-muted">
        Tool calls these match are approved without asking. Command rules match the command's first words and never
        approve chained (<span className="font-mono">;</span>, <span className="font-mono">&&</span>,{" "}
        <span className="font-mono">|</span>) or redirected commands.
      </div>
      <table className="mt-3 w-full text-xs">
        <thead className="text-left text-muted">
          <tr>
            <th className="pb-1 font-normal">Folder</th>
            <th className="pb-1 font-normal">Tool</th>
            <th className="pb-1 font-normal">Command starts with</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {rules.map((r) => (
            <tr key={r.id} className="border-t border-border">
              <td className="max-w-56 truncate py-1.5 pr-2 font-mono" title={r.cwd}>
                {r.cwd === "*" ? "All folders" : r.cwd}
              </td>
              <td className="py-1.5 pr-2">{r.toolKind}</td>
              <td className="py-1.5 pr-2 font-mono">{r.pattern || (r.toolKind === "execute" ? "any simple command" : "—")}</td>
              <td className="py-1.5 text-right">
                <button
                  className="text-muted hover:text-danger"
                  onClick={async () => {
                    await api.deletePermissionRule(r.id);
                    reload();
                  }}
                  aria-label="Delete rule"
                >
                  ×
                </button>
              </td>
            </tr>
          ))}
          {rules.length === 0 && (
            <tr>
              <td colSpan={4} className="py-2 text-muted">
                No rules yet. Use "Always allow" on a permission prompt, or add one here.
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <div className="mt-3 flex items-center gap-2">
        <select
          className={`${input} w-56 text-xs`}
          value={draft.cwd}
          onChange={(e) => setDraft({ ...draft, cwd: e.target.value })}
          aria-label="Folder"
        >
          <option value="">All folders</option>
          {folders.map((f) => (
            <option key={f} value={f}>
              {f}
            </option>
          ))}
        </select>
        <select
          className={`${input} w-28 text-xs`}
          value={draft.toolKind}
          onChange={(e) => setDraft({ ...draft, toolKind: e.target.value })}
          aria-label="Tool kind"
        >
          {KINDS.map((k) => (
            <option key={k}>{k}</option>
          ))}
        </select>
        {draft.toolKind === "execute" && (
          <input
            className={`${input} w-40 font-mono text-xs`}
            placeholder="e.g. cargo test"
            value={draft.pattern}
            onChange={(e) => setDraft({ ...draft, pattern: e.target.value })}
            aria-label="Command prefix"
          />
        )}
        <button className="rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg" onClick={add}>
          Add rule
        </button>
      </div>
    </div>
  );
}
