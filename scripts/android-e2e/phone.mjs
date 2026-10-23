#!/usr/bin/env node
// Drives Patchbay Remote's webview in an Android emulator over the Chrome DevTools protocol,
// through `adb forward`. It works because a debug APK's WebView is debuggable.
//
//   phone.mjs join   --address HOST:PORT --name NAME     (passphrase in E2E_PASSPHRASE)
//   phone.mjs verify --seed FILE
//   phone.mjs text
//
// `adb` picks the device from ANDROID_SERIAL. Needs Node 22 or newer (global WebSocket, fetch).

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const APP = "dev.patchbay.remote";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function adb(...args) {
  return execFileSync("adb", args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function flags(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith("--")) throw new Error(`unexpected argument ${argv[i]}`);
    out[argv[i].slice(2)] = argv[i + 1];
  }
  return out;
}

export async function until(what, ms, probe) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      const v = await probe();
      if (v) return v;
    } catch (e) {
      if (e.fatal) throw e;
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}${last ? ` (${last.message})` : ""}`);
}

/** Attaches to the app's page. The returned `done` removes the adb port forward again. */
export async function attach() {
  const pid = await until("the app to run (is it installed and started?)", 40_000, () => {
    try {
      return adb("shell", "pidof", APP).split(/\s+/)[0];
    } catch {
      return null;
    }
  });
  const socket = `webview_devtools_remote_${pid}`;
  const port = await until("the app's WebView debugger", 30_000, () => {
    const unix = adb("shell", "cat", "/proc/net/unix");
    return unix.includes(`@${socket}`) ? adb("forward", "tcp:0", `localabstract:${socket}`) : null;
  });
  const page = await until("the app's page", 30_000, async () => {
    const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    return pages.find((p) => p.type === "page" && p.webSocketDebuggerUrl && !p.url.startsWith("about:"));
  });

  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", () => reject(new Error("cannot open the page's debugger socket")), { once: true });
  });
  let n = 0;
  const pending = new Map();
  ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    const p = pending.get(m.id);
    if (!p) return;
    pending.delete(m.id);
    if (m.error) p.reject(new Error(JSON.stringify(m.error)));
    else p.resolve(m.result);
  });
  const send = (method, params) =>
    new Promise((resolve, reject) => {
      const id = ++n;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });
  const evalJs = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? "page script failed");
    return r.result.value;
  };
  return {
    evalJs,
    send,
    text: () => evalJs("document.body.innerText"),
    done: () => {
      ws.close();
      try {
        adb("forward", "--remove", `tcp:${port}`);
      } catch {
        /* already gone */
      }
    },
  };
}

const setInput = (index, value) => `(() => {
  const el = document.querySelectorAll('input')[${index}];
  if (!el) throw new Error('no input ${index} on this screen');
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, ${JSON.stringify(value)});
  el.dispatchEvent(new Event('input', { bubbles: true }));
})()`;

export const clickButton = (label, exact) => `(() => {
  const want = ${JSON.stringify(label)};
  const b = [...document.querySelectorAll('button')].find((x) => {
    const t = x.textContent.trim();
    return ${exact ? "t === want" : "t.startsWith(want)"};
  });
  if (!b || b.disabled) return false;
  b.click();
  return true;
})()`;

const inMain = (t) => /\bChats\b/.test(t) && /\bCode\b/.test(t);

async function join(f) {
  const pass = process.env.E2E_PASSPHRASE;
  if (!pass) throw new Error("E2E_PASSPHRASE is not set");
  if (!f.address || !f.name) throw new Error("--address and --name are required");
  const page = await attach();
  try {
    const t = await until("the join screen, a code, or the lists", 30_000, async () => {
      const s = await page.text();
      return /Join your mesh|Approve this phone/.test(s) || inMain(s) ? s : null;
    });
    if (inMain(t)) return console.log("already joined");
    if (/Approve this phone/.test(t)) return console.log("waiting for approval");

    await page.evalJs(setInput(0, f.address));
    await page.evalJs(setInput(1, pass));
    await page.evalJs(setInput(2, f.name));
    await until("the Join button to enable", 5_000, () => page.evalJs(clickButton("Join", true)));
    // Either the code screen, the lists (the mesh needed no approval), or the app's red error box.
    await until("the phone to ask for approval", 30_000, async () => {
      const s = await page.text();
      if (/Approve this phone/.test(s) || inMain(s)) return true;
      const problem = await page.evalJs(`document.querySelector('[class*="text-danger"]')?.textContent ?? null`);
      if (problem) throw Object.assign(new Error(`the app says: ${problem}`), { fatal: true });
      return false;
    });
    console.log("asked to join");
  } finally {
    page.done();
  }
}

async function verify(f) {
  if (!f.seed) throw new Error("--seed is required");
  const seed = JSON.parse(readFileSync(f.seed, "utf8"));
  const page = await attach();
  const failures = [];
  try {
    await until("the Chats and Code lists", 60_000, async () => inMain(await page.text()));
    for (const [tab, want] of [
      ["Chats", [seed.chat.title]],
      ["Code", [seed.session.title, seed.meshName]],
    ]) {
      await until(`the ${tab} tab`, 10_000, () => page.evalJs(clickButton(tab, false)));
      for (const w of want) {
        try {
          await until(`"${w}" under ${tab}`, 30_000, async () => (await page.text()).toLowerCase().includes(w.toLowerCase()));
          console.log(`ok   ${tab}: ${w}`);
        } catch {
          failures.push(`${tab}: ${w}`);
          console.log(`FAIL ${tab}: ${w}`);
        }
      }
    }
  } finally {
    page.done();
  }
  if (failures.length) throw new Error(`the phone does not show: ${failures.join("; ")}`);
}

async function text() {
  const page = await attach();
  try {
    console.log(await page.text());
  } finally {
    page.done();
  }
}

// Importing this file (scenarios.mjs does) must not run the command line.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [cmd, ...rest] = process.argv.slice(2);
  const commands = { join, verify, text };
  if (!commands[cmd]) {
    console.error("usage: phone.mjs join|verify|text ...");
    process.exit(2);
  }
  commands[cmd](flags(rest)).catch((e) => {
    console.error(`phone.mjs ${cmd}: ${e.message}`);
    process.exit(1);
  });
}
