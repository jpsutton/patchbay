// On-device check of useStickyScroll: streams text into the dev harness page in Chrome on a
// running emulator/phone, reads the real scroll position over CDP, and uses real touch swipes.
//   pnpm dev:remote            # in another terminal (serves :1424)
//   node scripts/e2e-sticky-scroll.mjs
// Needs adb on PATH or in $ANDROID_HOME/platform-tools, and exactly one device attached.
import { execFileSync } from "node:child_process";
import { homedir } from "node:os";

const adb = process.env.ADB ?? `${process.env.ANDROID_HOME ?? `${homedir()}/Library/Android/sdk`}/platform-tools/adb`;
const sh = (...a) => execFileSync(adb, a, { encoding: "utf8" }).trim();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, msg) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${msg}`);
  if (!ok) failures.push(msg);
};

// An existing reverse (e.g. from `tauri android dev`) is left alone; adding a duplicate errors.
if (!sh("reverse", "--list").includes("tcp:1424")) sh("reverse", "tcp:1424", "tcp:1424");
sh("shell", "am", "force-stop", "com.android.chrome");
sh("shell", "am", "set-debug-app", "--persistent", "com.android.chrome");
// Skip Chrome's first-run screens so the harness page is what loads.
sh("shell", "sh", "-c", "echo 'chrome --disable-fre --no-first-run --no-default-browser-check' > /data/local/tmp/chrome-command-line");
sh("shell", "am", "start", "-a", "android.intent.action.VIEW", "-n", "com.android.chrome/com.google.android.apps.chrome.Main",
  "-d", "http://localhost:1424/sticky-harness.html?lines=400&every=80");
// tcp:0 lets adb pick a free host port (9222 is often taken by a desktop Chrome).
const cdp = sh("forward", "tcp:0", "localabstract:chrome_devtools_remote");

let page;
for (let i = 0; i < 40 && !page; i++) {
  await sleep(500);
  try {
    page = (await (await fetch(`http://127.0.0.1:${cdp}/json`)).json()).find((t) => t.type === "page" && t.url.includes("sticky-harness"));
  } catch {}
}
if (!page) throw new Error("harness page never appeared in Chrome (is `pnpm dev:remote` running?)");

const ws = new WebSocket(page.webSocketDebuggerUrl.replace(/:\d+\//, `:${cdp}/`));
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  pending.get(d.id)?.(d);
};
const state = () =>
  new Promise((resolve) => {
    const my = ++id;
    pending.set(my, (d) => resolve(d.result?.result?.value ?? null));
    ws.send(JSON.stringify({ id: my, method: "Runtime.evaluate", params: { expression: "window.__sticky && window.__sticky()", returnByValue: true } }));
  });

const { width, height } = Object.fromEntries(sh("shell", "wm", "size").match(/(\d+)x(\d+)/).slice(1).map((v, i) => [["width", "height"][i], Number(v)]));
const x = Math.round(width / 2);
const swipe = (fromY, toY, ms = 250) => sh("shell", "input", "swipe", String(x), String(fromY), String(x), String(toY), String(ms));

// Wait for content to overflow the viewport.
let s;
for (let i = 0; i < 100; i++) {
  s = await state();
  if (s && s.n > 40) break;
  await sleep(200);
}
console.log(`device dpr=${s?.dpr}`);

// 1. Pinned while streaming: the gap to the bottom stays under a few px for the whole run.
let worst = 0;
const gaps = [];
for (let i = 0; i < 40; i++) {
  s = await state();
  gaps.push(+s.gap.toFixed(1));
  worst = Math.max(worst, s.gap);
  await sleep(100);
}
if (worst >= 4) console.log("gaps:", gaps.join(" "));
check(worst < 4, `follows the stream while pinned (worst gap ${worst.toFixed(2)}px)`);

// 2. A real upward touch scroll detaches and the view stops following.
swipe(Math.round(height * 0.35), Math.round(height * 0.75));
await sleep(600);
const afterSwipe = await state();
await sleep(2000);
const later = await state();
check(afterSwipe.gap > 100, `swipe up detaches (gap ${afterSwipe.gap.toFixed(0)}px)`);
check(later.n > afterSwipe.n && later.gap >= afterSwipe.gap - 2, `stays detached while content streams (n ${afterSwipe.n}->${later.n}, gap ${later.gap.toFixed(0)}px)`);

// 3. Scrolling back to the bottom re-pins.
for (let i = 0; i < 40 && (await state()).gap > 40; i++) swipe(Math.round(height * 0.8), Math.round(height * 0.2), 120);
await sleep(1500);
worst = 0;
for (let i = 0; i < 20; i++) {
  s = await state();
  worst = Math.max(worst, s.gap);
  await sleep(100);
}
check(worst < 4, `re-pins after scrolling to the bottom (worst gap ${worst.toFixed(2)}px)`);

ws.close();
sh("shell", "rm", "-f", "/data/local/tmp/chrome-command-line");
sh("shell", "am", "force-stop", "com.android.chrome");
process.exit(failures.length ? 1 : 0);
