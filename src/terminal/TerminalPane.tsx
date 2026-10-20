import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { api } from "../lib/ipc";
import { useStore } from "../lib/store";

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
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

  return <div ref={host} className="h-full w-full overflow-hidden bg-panel" />;
}
