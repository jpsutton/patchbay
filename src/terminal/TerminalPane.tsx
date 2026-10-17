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

export function TerminalPane({ id, cwd, visible }: { id: string; cwd: string; visible: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const fitRef = useRef<FitAddon | null>(null);

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
    const termId = `term-${id}`;
    let closed = false;
    api
      .termOpen(
        termId,
        cwd,
        term.cols || 80,
        term.rows || 24,
        (e) => {
          if (e.type === "data") term.write(b64ToBytes(e.data));
          else term.write(`\r\n\x1b[2m[process exited${e.code != null ? ` with code ${e.code}` : ""}]\x1b[0m\r\n`);
        },
        id,
      )
      .catch((err) => term.write(`\x1b[31m${String(err)}\x1b[0m\r\n`));
    const onData = term.onData((d) => {
      if (!closed) api.termWrite(termId, d).catch(() => {});
    });
    const onResize = term.onResize(({ cols, rows }) => {
      if (!closed) api.termResize(termId, cols, rows).catch(() => {});
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
  }, [id, cwd]);

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
