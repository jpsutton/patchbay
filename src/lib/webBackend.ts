// The browser's stand-in for Tauri's IPC when patchbayd serves the UI: `invoke` calls, channel
// messages and app events travel over one WebSocket to the daemon (src-tauri/src/web.rs). The rest
// of the UI keeps using @tauri-apps/api as in the desktop app.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

const TOKEN_KEY = "patchbay.webToken";

type Pending = { resolve: (v: unknown) => void; reject: (e: unknown) => void };

interface Internals {
  runCallback: (id: number, data: unknown) => void;
}

/** The token from `#token=…` (then dropped from the address bar), else the one saved earlier. */
function takeToken(): string | null {
  const m = /(?:^#|&)token=([^&]+)/.exec(location.hash);
  if (m) {
    const t = decodeURIComponent(m[1]);
    try {
      localStorage.setItem(TOKEN_KEY, t);
    } catch {
      /* storage unavailable: it lasts this page only */
    }
    history.replaceState(null, "", location.pathname + location.search);
    return t;
  }
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

function wsUrl(): string {
  return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`;
}

/** Open a socket and sign in; resolves once the daemon says it's ready. */
function connect(token: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl());
    let settled = false;
    ws.onopen = () => ws.send(JSON.stringify({ auth: token }));
    ws.onmessage = (e) => {
      if (settled) return;
      const m = JSON.parse(e.data as string);
      settled = true;
      if (m.ready) resolve(ws);
      else reject(new AuthError(m.authError ?? "sign-in refused"));
    };
    ws.onerror = () => {
      if (!settled) {
        settled = true;
        reject(new Error("can't reach patchbayd"));
      }
    };
    ws.onclose = () => {
      if (!settled) {
        settled = true;
        reject(new Error("patchbayd closed the connection"));
      }
    };
  });
}

class AuthError extends Error {}

/** A bare page asking for the token, shown before the app when there is none or it's wrong. */
function askToken(message: string): Promise<string> {
  return new Promise((resolve) => {
    const root = document.getElementById("root")!;
    root.innerHTML = `
      <form style="max-width:420px;margin:15vh auto;padding:24px;font:14px system-ui,sans-serif;display:flex;flex-direction:column;gap:12px">
        <h1 style="font-size:18px;margin:0">Patchbay</h1>
        <p style="margin:0;opacity:.75"></p>
        <input name="token" type="password" autocomplete="off" placeholder="Access token" autofocus
          style="padding:8px 10px;border:1px solid #8886;border-radius:8px;font:inherit;background:transparent;color:inherit" />
        <button style="padding:8px 10px;border-radius:8px;border:0;background:#4f7cff;color:#fff;font:inherit">Connect</button>
        <p style="margin:0;opacity:.6;font-size:12.5px">Run <code>patchbayd token</code> on the computer running Patchbay to get the address with its token.</p>
      </form>`;
    root.querySelector("p")!.textContent = message;
    root.querySelector("form")!.addEventListener("submit", (e) => {
      e.preventDefault();
      const t = (root.querySelector("input") as HTMLInputElement).value.trim();
      if (!t) return;
      try {
        localStorage.setItem(TOKEN_KEY, t);
      } catch {
        /* storage unavailable */
      }
      root.innerHTML = "";
      resolve(t);
    });
  });
}

async function signIn(): Promise<WebSocket> {
  let token = takeToken();
  let message = "Enter the access token for this Patchbay.";
  for (;;) {
    if (!token) token = await askToken(message);
    try {
      return await connect(token);
    } catch (e) {
      if (!(e instanceof AuthError)) throw e;
      token = null;
      message = "That token didn't work. Enter the current one.";
    }
  }
}

/** A banner while the daemon is unreachable; reloads once it's back, since streams didn't survive. */
function onDisconnect() {
  const bar = document.createElement("div");
  bar.textContent = "Lost the connection to patchbayd. Reconnecting…";
  bar.setAttribute("role", "status");
  Object.assign(bar.style, {
    position: "fixed",
    inset: "0 0 auto 0",
    zIndex: "9999",
    padding: "8px 12px",
    font: "13px system-ui, sans-serif",
    textAlign: "center",
    background: "#b42318",
    color: "#fff",
  });
  document.body.appendChild(bar);
  const retry = () => {
    const ws = new WebSocket(wsUrl());
    ws.onopen = () => {
      ws.close();
      location.reload();
    };
    ws.onerror = () => setTimeout(retry, 2000);
  };
  setTimeout(retry, 1000);
}

export async function install() {
  (window as { __PATCHBAY_WEB__?: boolean }).__PATCHBAY_WEB__ = true;
  mockWindows("main");
  const ws = await signIn();

  let nextId = 1;
  const pending = new Map<number, Pending>();
  // Per channel, the index of its next message: Tauri's Channel delivers in index order.
  const chanIndex = new Map<number, number>();
  const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;

  ws.onmessage = (e) => {
    const m = JSON.parse(e.data as string);
    if ("id" in m) {
      const p = pending.get(m.id);
      pending.delete(m.id);
      if (!p) return;
      if ("err" in m) p.reject(m.err);
      else p.resolve(m.ok);
    } else if ("chan" in m) {
      const index = chanIndex.get(m.chan) ?? 0;
      chanIndex.set(m.chan, index + 1);
      internals().runCallback(m.chan, { index, message: m.msg });
    } else if ("event" in m) {
      void emit(m.event, m.payload);
    }
  };
  ws.onclose = () => {
    for (const p of pending.values()) p.reject("lost the connection to patchbayd");
    pending.clear();
    onDisconnect();
  };

  mockIPC(
    (cmd, args) => {
      // Native-only plugins (dialogs, opener, notifications, window and webview control) have
      // browser fallbacks at their call sites; answer anything left over harmlessly.
      if (cmd.startsWith("plugin:")) return null;
      return new Promise((resolve, reject) => {
        const id = nextId++;
        pending.set(id, { resolve, reject });
        // Channels serialize as `__CHANNEL__:<id>` (their toJSON), as over Tauri's IPC.
        ws.send(JSON.stringify({ id, cmd, args: args ?? {} }));
      });
    },
    { shouldMockEvents: true },
  );
}
