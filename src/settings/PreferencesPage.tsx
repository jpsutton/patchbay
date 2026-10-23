import { useEffect, useState, type ReactNode } from "react";
import type { Preferences } from "../lib/ipc";
import { useStore } from "../lib/store";
import { isWeb } from "../lib/platform";
import { Toggle } from "../components/Toggle";
import { input } from "./ProviderPage";
import { DiagnosticsPage } from "./DiagnosticsPage";
import { McpPage } from "./McpPage";
import { MeshPage } from "./MeshPage";
import { PromptsPage } from "./PromptsPage";
import { UsagePage } from "./UsagePage";

export type PrefSection = "general" | "chat" | "prompts" | "agents" | "mcp" | "terminal" | "mesh" | "usage" | "diagnostics";

export const PREF_SECTIONS: { id: PrefSection; label: string }[] = [
  { id: "general", label: "General" },
  { id: "chat", label: "Chat" },
  { id: "prompts", label: "Prompts" },
  { id: "agents", label: "Coding agents" },
  { id: "mcp", label: "MCP servers" },
  { id: "terminal", label: "Terminal" },
  { id: "mesh", label: "Mesh" },
  { id: "usage", label: "Usage" },
  { id: "diagnostics", label: "Diagnostics" },
];

export function Row({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-6 border-b border-border py-3 last:border-b-0">
      <div className="min-w-0">
        <div className="text-sm">{label}</div>
        {hint && <div className="mt-0.5 text-xs text-muted">{hint}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

export function Choice<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
}) {
  return (
    <div className="flex rounded-md border border-border bg-panel p-0.5 text-xs" role="radiogroup">
      {options.map((o) => (
        <button
          key={o.value}
          role="radio"
          aria-checked={value === o.value}
          className={`rounded px-2.5 py-1 ${value === o.value ? "bg-bg text-fg shadow-sm" : "text-muted hover:text-fg"}`}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** Text/number field that saves on blur or Enter and shows the backend's validation error. */
export function Deferred({
  value,
  onCommit,
  placeholder,
  mono,
  width = "w-56",
}: {
  value: string;
  onCommit: (v: string) => Promise<string | null>;
  placeholder?: string;
  mono?: boolean;
  width?: string;
}) {
  const [draft, setDraft] = useState(value);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => setDraft(value), [value]);
  const commit = async () => {
    if (draft === value) return;
    const err = await onCommit(draft);
    setError(err);
    if (err) setDraft(value);
  };
  return (
    <div className="flex flex-col items-end gap-1">
      <input
        className={`${input} ${width} ${mono ? "font-mono" : ""}`}
        value={draft}
        placeholder={placeholder}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
      />
      {error && (
        <span className="max-w-56 text-right text-[12px] text-danger" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

export function LongText({
  value,
  onCommit,
  placeholder,
}: {
  value: string;
  onCommit: (v: string) => Promise<string | null>;
  placeholder: string;
}) {
  const [draft, setDraft] = useState(value);
  const [state, setState] = useState<"idle" | "saved" | string>("idle");
  useEffect(() => setDraft(value), [value]);
  return (
    <div className="flex flex-col gap-1 py-3">
      <textarea
        className={`${input} h-32 resize-y font-mono text-xs`}
        value={draft}
        placeholder={placeholder}
        onChange={(e) => {
          setDraft(e.target.value);
          setState("idle");
        }}
        onBlur={async () => {
          if (draft === value) return;
          const err = await onCommit(draft);
          setState(err ?? "saved");
        }}
      />
      <div className="h-4 text-[12px]">
        {state === "saved" ? (
          <span className="text-ok">Saved</span>
        ) : state !== "idle" ? (
          <span className="text-danger">{state}</span>
        ) : (
          <span className="text-muted">Saved when you click away.</span>
        )}
      </div>
    </div>
  );
}

const MOD = navigator.platform.toLowerCase().includes("mac") ? "⌘" : "Ctrl";

export function PreferencesPage({ section }: { section: PrefSection }) {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const setToast = useStore((s) => s.setToast);
  const set = async <K extends keyof Preferences>(k: K, v: Preferences[K]) => {
    const err = await update({ [k]: v } as Partial<Preferences>);
    if (err) setToast(err);
  };

  if (section === "general") {
    return (
      <div className="flex max-w-xl flex-col">
        <Row label="Theme">
          <Choice
            value={prefs.theme}
            onChange={(v) => set("theme", v)}
            options={[
              { value: "system", label: "System" },
              { value: "light", label: "Light" },
              { value: "dark", label: "Dark" },
            ]}
          />
        </Row>
        <Row label="Text size">
          <Choice
            value={prefs.textSize}
            onChange={(v) => set("textSize", v)}
            options={[
              { value: "small", label: "Small" },
              { value: "default", label: "Default" },
              { value: "large", label: "Large" },
            ]}
          />
        </Row>
        <Row
          label="Send messages with"
          hint={`Shift+Enter inserts a new line, and so does Enter when ${MOD}+Enter sends. While a Code agent works, ${MOD}+Enter (${MOD}+Shift+Enter when ${MOD}+Enter sends) stops its turn and sends now. On the phone app, Enter always inserts a line — send with the button.`}
        >
          <Choice
            value={prefs.sendKey}
            onChange={(v) => set("sendKey", v)}
            options={[
              { value: "enter", label: "Enter" },
              { value: "mod-enter", label: `${MOD}+Enter` },
            ]}
          />
        </Row>
        <Row label="Open on launch">
          <Choice
            value={prefs.launchTab}
            onChange={(v) => set("launchTab", v)}
            options={[
              { value: "last", label: "Last used" },
              { value: "chat", label: "Chat" },
              { value: "code", label: "Code" },
            ]}
          />
        </Row>
        {/* The browser draws its own frame. */}
        {!isWeb() && (
        <Row
          label="Title bar"
          hint={
            prefs.titleBar === "app"
              ? "Patchbay draws minimize, maximize and close itself. Drag the top bars to move the window; double-click them to maximize."
              : "Your window manager draws the title bar, with its own buttons and theme."
          }
        >
          <Choice
            value={prefs.titleBar}
            onChange={(v) => set("titleBar", v)}
            options={[
              { value: "system", label: "System" },
              { value: "app", label: "Patchbay" },
            ]}
          />
        </Row>
        )}
        <Row label="Notifications" hint="When a reply or agent turn finishes, or the agent needs approval, while Patchbay is in the background.">
          <Toggle label="Notifications" checked={prefs.notifications} onChange={(v) => set("notifications", v)} />
        </Row>
        {/* Global shortcuts belong to the desktop app. */}
        {!isWeb() && (
        <Row
          label="Quick entry shortcut"
          hint={
            <>
              Global shortcut that opens a small prompt window from anywhere, e.g. <span className="font-mono">Alt+Space</span>{" "}
              or <span className="font-mono">CommandOrControl+Shift+Space</span>. Blank turns it off. On Wayland the
              desktop may ask you to confirm it, and its own shortcut settings have the final say.
            </>
          }
        >
          <Deferred
            value={prefs.quickEntryShortcut}
            placeholder="Off"
            mono
            width="w-48"
            onCommit={(v) => update({ quickEntryShortcut: v.trim() })}
          />
        </Row>
        )}
      </div>
    );
  }

  if (section === "prompts") return <PromptsPage />;
  if (section === "mcp") return <McpPage />;
  if (section === "mesh") return <MeshPage />;
  if (section === "usage") return <UsagePage />;
  if (section === "diagnostics") return <DiagnosticsPage />;

  if (section === "chat") {
    return (
      <div className="flex max-w-xl flex-col">
        <Row label="Expand reasoning by default" hint="Show a model's thinking without clicking to open it.">
          <Toggle label="Expand reasoning by default" checked={prefs.expandReasoning} onChange={(v) => set("expandReasoning", v)} />
        </Row>
        <Row
          label="Generate conversation titles"
          hint="After the first reply, ask the same model for a short title. Code sessions whose agent doesn't name them get one too, after the first and third turns. Each costs one small extra request."
        >
          <Toggle label="Generate conversation titles" checked={prefs.generateTitles} onChange={(v) => set("generateTitles", v)} />
        </Row>
        <div className="pt-3">
          <div className="text-sm">Default system prompt</div>
          <div className="mt-0.5 text-xs text-muted">
            Copied into each new conversation. You can still change it per conversation.
          </div>
          <LongText
            value={prefs.defaultSystemPrompt}
            placeholder="e.g. You are a concise assistant. Prefer short answers with code examples."
            onCommit={(v) => update({ defaultSystemPrompt: v })}
          />
        </div>
      </div>
    );
  }

  return (
    <div className="flex max-w-xl flex-col">
      <Row label="Shell" hint="Absolute path. Blank uses your login shell.">
        <Deferred
          value={prefs.terminalShell}
          placeholder="/bin/zsh"
          mono
          onCommit={(v) => update({ terminalShell: v.trim() })}
        />
      </Row>
      <Row label="Font size">
        <Deferred
          value={String(prefs.terminalFontSize)}
          mono
          width="w-24"
          onCommit={(v) => {
            if (!/^\d+$/.test(v.trim())) return Promise.resolve("Enter a whole number");
            return update({ terminalFontSize: Number(v.trim()) });
          }}
        />
      </Row>
      <Row
        label="Local echo"
        hint="Show what you type right away, dimmed, before the shell echoes it back. Auto turns it on when the echo is slow, such as for a session on another computer."
      >
        <Choice
          value={prefs.terminalLocalEcho}
          onChange={(v) => update({ terminalLocalEcho: v })}
          options={[
            { value: "auto", label: "Auto" },
            { value: "on", label: "Always" },
            { value: "off", label: "Off" },
          ]}
        />
      </Row>
      <p className="pt-3 text-xs text-muted">Applies to terminals opened after the change.</p>
    </div>
  );
}
