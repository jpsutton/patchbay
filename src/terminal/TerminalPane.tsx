import { useEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { ContextMenu, type ItemAction } from "../components/ItemMenu";
import { copyText, pasteText } from "../lib/clipboard";
import { api, errorText } from "../lib/ipc";
import { isMod } from "../lib/shortcuts";
import { useStore } from "../lib/store";

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Copy the terminal's selection, if any. */
function copySelection(term: Terminal) {
  const text = term.getSelection();
  if (text) copyText(text).catch((e) => useStore.getState().setToast(`Could not copy: ${errorText(e)}`));
}

/** Paste the clipboard into the terminal, bracketed when the program asked for that. */
function pasteInto(term: Terminal) {
  pasteText()
    .then((text) => text && term.paste(text))
    .catch((e) => useStore.getState().setToast(`Could not paste: ${errorText(e)}`));
}

function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/**
 * A terminal for session `id`, on the computer that runs it: the user's shell, or with
 * `authMethod` the agent's sign-in command for that method, which `onExit` hears end.
 */
export function TerminalPane({
  id,
  cwd,
  visible,
  authMethod,
  onExit,
}: {
  id: string;
  cwd: string;
  visible: boolean;
  authMethod?: string;
  onExit?: (code: number | null) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; actions: ItemAction[] } | null>(null);
  const onExitRef = useRef(onExit);
  useEffect(() => {
    onExitRef.current = onExit;
  });

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const term = new Terminal({
      fontFamily: cssVar("--font-mono") || "monospace",
      fontSize: useStore.getState().prefs.terminalFontSize,
      cursorBlink: true,
      allowProposedApi: true,
      theme: {
        background: cssVar("--color-panel"),
        foreground: cssVar("--color-fg"),
        cursor: cssVar("--color-accent"),
        selectionBackground: cssVar("--color-border"),
      },
    });
    termRef.current = term;
    // Ctrl+C is the shell's interrupt, so copy and paste are Ctrl+Shift+C and Ctrl+Shift+V, as in
    // other terminals (⌘C and ⌘V on macOS reach the browser's own copy and paste). The keys stop
    // here: the shell doesn't see them, and neither does the app's Ctrl+Shift+C (copy last reply).
    term.attachCustomKeyEventHandler((e) => {
      if (!isMod(e) || !e.shiftKey || e.altKey) return true;
      const k = e.key.toLowerCase();
      if (k !== "c" && k !== "v") return true;
      e.preventDefault();
      e.stopPropagation();
      if (e.type === "keydown") {
        if (k === "c") copySelection(term);
        else pasteInto(term);
      }
      return false;
    });
    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    term.open(el);
    try {
      fit.fit();
    } catch {
      /* hidden */
    }
    const termId = authMethod ? `auth-${id}` : `term-${id}`;
    let closed = false;
    // Bumped per spawn, so a replaced shell's late events are ignored.
    let gen = 0;
    // A user shell that has exited waits for Enter to start a new one, and input waits until
    // the new one is open.
    let shell: "starting" | "open" | "exited" = "starting";
    const start = () => {
      const mine = ++gen;
      shell = "starting";
      api
        .termOpen(
          termId,
          cwd,
          term.cols || 80,
          term.rows || 24,
          (e) => {
            if (closed || mine !== gen) return;
            if (e.type === "data") term.write(b64ToBytes(e.data));
            else {
              const how = e.code != null ? ` with code ${e.code}` : "";
              if (authMethod) {
                term.write(`\r\n\x1b[2m[process exited${how}]\x1b[0m\r\n`);
                onExitRef.current?.(e.code);
              } else {
                term.write(`\r\n\x1b[2m[process exited${how}; press Enter for a new shell]\x1b[0m\r\n`);
                shell = "exited";
              }
            }
          },
          id,
          authMethod,
        )
        .then(() => {
          if (mine !== gen || shell !== "starting") return;
          shell = "open";
          // The window may have been resized while it started.
          if (!closed) api.termResize(termId, term.cols, term.rows).catch(() => {});
        })
        .catch((err) => {
          if (closed || mine !== gen) return;
          term.write(`\x1b[31m${String(err)}\x1b[0m\r\n`);
          if (authMethod) onExitRef.current?.(null);
          else shell = "exited";
        });
    };
    start();
    const onData = term.onData((d) => {
      if (closed) return;
      if (shell === "exited") {
        if (d === "\r") start();
        return;
      }
      if (shell === "open") api.termWrite(termId, d).catch(() => {});
    });
    const onResize = term.onResize(({ cols, rows }) => {
      if (!closed && shell === "open") api.termResize(termId, cols, rows).catch(() => {});
    });
    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        /* hidden */
      }
    });
    ro.observe(el);
    return () => {
      closed = true;
      ro.disconnect();
      onData.dispose();
      onResize.dispose();
      api.termClose(termId).catch(() => {});
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [id, cwd, authMethod]);

  useEffect(() => {
    if (visible)
      requestAnimationFrame(() => {
        try {
          fitRef.current?.fit();
        } catch {
          /* ignore */
        }
      });
  }, [visible]);

  const openMenu = (e: ReactMouseEvent) => {
    const term = termRef.current;
    if (!term) return;
    e.preventDefault();
    const done = (f: (t: Terminal) => void) => () => {
      f(term);
      term.focus();
    };
    const actions: ItemAction[] = [];
    if (term.hasSelection()) actions.push({ label: "Copy", onSelect: done(copySelection) });
    actions.push({ label: "Paste", onSelect: done(pasteInto) });
    setMenu({ x: e.clientX, y: e.clientY, actions });
  };

  return (
    <>
      <div ref={host} className="h-full w-full overflow-hidden bg-panel" onContextMenu={openMenu} />
      {menu && <ContextMenu x={menu.x} y={menu.y} title="Terminal" actions={menu.actions} onClose={() => setMenu(null)} />}
    </>
  );
}
