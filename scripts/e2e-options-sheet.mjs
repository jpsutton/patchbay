// Keyboard-only check of the Composer's options sheet (pb-20261110-11) in headless Chrome, driven
// over CDP with real key events. Chromium only: no WebKit or Windows claim.
//   pnpm dev   # in another terminal (serves :1420), or set PB_URL=http://localhost:<port>
//   node scripts/e2e-options-sheet.mjs
// Chrome: $CHROME, else the macOS app path.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const url = `${process.env.PB_URL ?? "http://localhost:1420"}/options-sheet-harness.html`;
const chromeBin = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, msg) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${msg}`);
  if (!ok) failures.push(msg);
};

const dir = mkdtempSync(join(tmpdir(), "pb-options-sheet-"));
const chrome = spawn(chromeBin, ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${dir}`, "--no-first-run", "--window-size=400,800", "about:blank"], { stdio: "ignore" });
const done = () => {
  chrome.kill();
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch {}
};
process.on("exit", done);

let port;
for (let i = 0; i < 60 && !port; i++) {
  await sleep(250);
  try {
    port = readFileSync(join(dir, "DevToolsActivePort"), "utf8").split("\n")[0];
  } catch {}
}
if (!port) throw new Error("Chrome never opened its DevTools port");
const target = await (await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: "PUT" })).json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && pending.has(d.id)) pending.get(d.id)(d);
};
const send = (method, params = {}) =>
  new Promise((res) => {
    const n = ++id;
    pending.set(n, res);
    ws.send(JSON.stringify({ id: n, method, params }));
  });
const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result.result.value;

const KEYS = { Tab: [9, "Tab", ""], Escape: [27, "Escape", ""], Enter: [13, "Enter", "\r"] };
async function press(key, mods = 0) {
  const [vk, code, text] = KEYS[key] ?? [key.charCodeAt(0), `Key${key.toUpperCase()}`, key];
  const base = { key, code, windowsVirtualKeyCode: vk, modifiers: mods };
  await send("Input.dispatchKeyEvent", { type: text ? "keyDown" : "rawKeyDown", text: text || undefined, ...base });
  await send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
  await sleep(60);
}
const SHIFT = 8, CTRL = 2;
const where = () => ev(`(() => { const a = document.activeElement; const d = document.querySelector('[role=dialog]'); return { id: a?.id || a?.getAttribute('aria-label') || a?.tagName, inDialog: !!(d && d.contains(a)), isDialog: a === d, open: !!d }; })()`);

await send("Page.enable");
// A headless page is not focused by default; without this the key events reach nothing.
await send("Emulation.setFocusEmulationEnabled", { enabled: true });
for (let i = 0; i < 80 && !(await ev(`!!document.querySelector('[aria-haspopup=dialog]')`)); i++) await sleep(250);
await send("Emulation.setDeviceMetricsOverride", { width: 360, height: 700, deviceScaleFactor: 1, mobile: false });
await sleep(300);

// Keyboard only: focus the trigger (as Tab would land on it), then Enter opens the sheet.
await ev(`document.querySelector('[aria-haspopup=dialog]').focus()`);
await ev(`window.__leaked = []; window.addEventListener('keydown', (e) => window.__leaked.push(e.key + (e.ctrlKey ? '+ctrl' : '')))`);
await press("Enter");
let w = await where();
check(w.open, "Enter on the options button opens the sheet");
check((await ev(`document.querySelector('[role=dialog]')?.getAttribute('aria-modal')`)) === "true", "the dialog has aria-modal=true");
check(w.inDialog && !w.isDialog, `focus enters the sheet on open (on ${w.id})`);

let stayed = true, seen = new Set();
for (let i = 0; i < 8; i++) {
  await press("Tab");
  w = await where();
  seen.add(w.id);
  stayed &&= w.inDialog;
}
check(stayed, `Tab x8 never leaves the sheet (visited: ${[...seen].join(", ")})`);
for (let i = 0; i < 8; i++) {
  await press("Tab", SHIFT);
  w = await where();
  stayed &&= w.inDialog;
}
check(stayed, "Shift+Tab x8 never leaves the sheet");
check(!seen.has("behind-a") && !seen.has("behind-b"), "the controls behind the sheet are never reached");

await ev(`window.__leaked = []`);
await press("]", CTRL);
check((await ev(`window.__leaked.length`)) === 0, "Ctrl+] is stopped at the sheet (the app's shortcuts do not see it)");

await press("Escape");
w = await where();
check(!w.open, "Escape closes the sheet");
check((await ev(`document.activeElement === document.querySelector('[aria-haspopup=dialog]')`)), "focus returns to the options button after Escape");

// Close through Done, by keyboard: Tab to it, Enter.
await press("Enter");
check((await where()).open, "Enter reopens the sheet");
for (let i = 0; i < 10 && (await ev(`document.activeElement?.textContent`)) !== "Done"; i++) await press("Tab");
await press("Enter");
check(!(await where()).open, "Enter on Done closes the sheet");
check((await ev(`document.activeElement === document.querySelector('[aria-haspopup=dialog]')`)), "focus returns to the options button after Done");

ws.close();
done();
if (failures.length) {
  console.error(`${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("options sheet keyboard checks passed");
