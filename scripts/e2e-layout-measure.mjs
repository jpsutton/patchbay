// Narrow-pane layout measurement with hard assertions (pb-20261110-10). Opens the dev page (fake
// backend) in headless Chrome, puts 4 sessions side by side, injects a busy pane with queued agent
// messages and 9-digit token totals, then at pane widths 180/250/340/500 px reports anything under
// the composer column (and the working line) whose visible right edge is past its pane's, and where the
// header's mode and approval controls end up (with the shared chip's minimum width and without it,
// also for a single un-split pane). The
// result goes through `layout-assert.mjs`: a failed, empty or half-found measurement FAILS instead
// of reading as a pass, so the check can't pass on nothing.
//
//   pnpm dev                                         # serves :1420, or set PB_URL=http://localhost:<port>
//   node scripts/e2e-layout-measure.mjs              # measure and assert; exit 1 on any problem
//   node scripts/e2e-layout-measure.mjs --plant empty|noprobe|overflow|nomode|bigfloor   # a planted failure: must exit 1
//   node scripts/e2e-layout-measure.mjs --self-test  # clean run passes, every plant fails
//
// Chrome: $CHROME, else the macOS app path (throwaway profile, mock keychain). Chromium only. The
// selectors below (data-pane, data-composer, the Queued messages and Token usage labels) are the
// ones the narrow-pane work uses; when a layout change renames one, this fails loudly, by design.
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { SINGLE_WIDTHS, WIDTHS, assertLayout, layoutProblems } from "./layout-assert.mjs";

const PLANTS = {
  empty: "no panes",
  noprobe: "usage line was not found",
  overflow: "past the pane edge",
  nomode: "mode control was not found",
  bigfloor: "the chip's floor",
};
const argv = process.argv.slice(2);
const plantIdx = argv.indexOf("--plant");
const plant = plantIdx >= 0 ? argv[plantIdx + 1] : undefined;
if (plantIdx >= 0 && !(plant in PLANTS)) throw new Error(`--plant takes one of ${Object.keys(PLANTS).join(", ")}`);

if (argv.includes("--self-test")) {
  const self = fileURLToPath(import.meta.url);
  let bad = 0;
  const run = (args) => spawnSync(process.execPath, [self, ...args], { encoding: "utf8", env: process.env });
  const clean = run([]);
  console.log(`clean run: exit ${clean.status} (expected 0)`);
  if (clean.status !== 0) {
    bad++;
    console.log(clean.stdout.slice(-1500), clean.stderr.slice(-800));
  }
  for (const [name, expect] of Object.entries(PLANTS)) {
    const r = run(["--plant", name]);
    const red = r.status === 1 && `${r.stdout}${r.stderr}`.includes(expect);
    console.log(`plant ${name}: exit ${r.status} (expected 1), names "${expect}": ${red ? "yes" : "NO"}`);
    if (!red) {
      bad++;
      console.log(r.stdout.slice(-1200), r.stderr.slice(-600));
    }
  }
  console.log(bad ? `self-test FAILED (${bad})` : "self-test passed: the clean run is green and every planted failure is red");
  process.exit(bad ? 1 : 0);
}

// Runs inside the page. Returns the structured measurement; `plant` breaks one probe on purpose.
async function measureInPage(widths, singleWidths, plant) {
  const { useStore } = await import("/src/lib/store.ts");
  const titles = ["Windows installer testing", "Claude plan preview", "Codex prompt migration", "OpenCode refactor"];
  const ids = titles.map((t) => useStore.getState().codeSessions.find((s) => s.title === t)?.id);
  if (ids.some((id) => !id)) throw new Error(`fake sessions not found: ${JSON.stringify(ids)}`);
  if (useStore.getState().panes.join() !== ids.join()) {
    useStore.setState({ mode: "code", sidebarCollapsed: true, panes: [], activeCodeSessionId: null });
    useStore.getState().selectCodeSession(ids[0]);
    for (const id of ids.slice(1)) useStore.getState().openInSplit(id, "right");
    await new Promise((r) => setTimeout(r, 3000));
  }
  const node = "3816e21ff9bdf3ca5746252005609a1b";
  const agentText = (title) =>
    `Patchbay agent message from Code session '${title}' (9c1a0b0b-e9c0-401b-8351-78c4da92e0d7) on LT02 (${node}):\n\nUnderstood, the build is ready for install and the seat is closed.\n\nTo reply, use the Patchbay send_agent_message tool and target session 9c1a0b0b.`;
  const big = {
    totals: { input: 317_800_000, cachedRead: 312_700_000, cacheWrite: 0, output: 588_000, reasoning: 0, costUsd: 162.07, apiMs: 0 },
    contextTokens: 190000,
    contextWindow: 1000000,
    autoCompactPercent: 85,
  };
  const inject = () =>
    useStore.setState((st) => {
      const code = { ...st.code };
      const p = (i, patch) => (code[ids[i]] = { ...code[ids[i]], ...patch });
      p(0, {
        busy: true,
        turnStartedAt: Date.now() - 25_140_000,
        turnChars: 10800,
        lastForegroundAt: Date.now(),
        lastPhase: "writing",
        usage: big,
        queued: [
          { promptId: "q1", text: agentText("Triage Worker - C1 APK build (resumed)"), origin: `agent:${node}:9c1a0b0b` },
          { promptId: "q2", text: agentText("Triage Ship Lead - Sonnet"), origin: `agent:${node}:87058a23` },
          { promptId: "q3", text: "a plain queued message typed by a person that is fairly long" },
        ],
      });
      // More than three agent modes make the header's mode control the shared chip (ChipSelect), the
      // one with a minimum width; the fake backend's own modes (two) draw a radio group instead.
      const fiveModes = {
        currentModeId: "build",
        availableModes: ["build", "plan", "review", "debug", "explore"].map((id) => ({ id, name: id[0].toUpperCase() + id.slice(1) })),
      };
      p(1, { modes: fiveModes, usage: { ...big, totals: { ...big.totals, input: 31_600_000, cachedRead: 29_300_000, output: 150_000 } } });
      // Pane 3 never started an agent here, so it has no modes: give it the fake backend's two.
      p(2, { modes: { currentModeId: "build", availableModes: [{ id: "build", name: "Build" }, { id: "plan", name: "Plan" }] }, usage: big });
      p(3, { busy: true, turnStartedAt: Date.now() - 48_000, turnChars: 2200, lastForegroundAt: Date.now(), lastPhase: "thinking", usage: big });
      return { code };
    });
  // Planted failures break the measurement or the layout on purpose, to prove the check goes red.
  const paneSel = plant === "empty" ? "[data-pane-planted-missing]" : "[data-pane]";
  const usageSel = plant === "noprobe" ? '[aria-label="planted missing"]' : '[aria-label="Token usage"]';
  const measure = () =>
    [...document.querySelectorAll(paneSel)].map((pane) => {
      const pr = pane.getBoundingClientRect();
      const ta = pane.querySelector("textarea[data-composer]");
      const col = ta?.closest('[class*="max-w-[var(--chat-width)]"]');
      const chipRow = pane.querySelector('[aria-label="Attach files"]')?.parentElement;
      const wk = pane.querySelector("[data-working-kind]");
      // An element inside a clipping ancestor is only visible up to that ancestor's edge.
      const visRight = (el) => {
        let r = el.getBoundingClientRect().right;
        for (let a = el.parentElement; a && a !== pane; a = a.parentElement)
          if (getComputedStyle(a).overflowX !== "visible") r = Math.min(r, a.getBoundingClientRect().right);
        return r;
      };
      const past = [];
      for (const root of [col, wk].filter(Boolean))
        for (const el of [root, ...root.querySelectorAll("*")]) {
          const r = el.getBoundingClientRect();
          if (!r.width || !r.height) continue;
          const d = Math.round((visRight(el) - pr.right) * 10) / 10;
          if (d <= 0.5) continue;
          // The composer's chip row colliding with itself is a separate row (pb-20261109-17).
          if (chipRow?.contains(el)) continue;
          past.push(`${el.tagName.toLowerCase()}${el.getAttribute("aria-label") ? `[${el.getAttribute("aria-label")}]` : ""} +${d}px`);
        }
      const li = pane.querySelector('ul[aria-label="Queued messages"] li');
      const u = pane.querySelector(usageSel);
      const tot = u?.firstElementChild;
      const ring = u?.querySelector("svg")?.getBoundingClientRect();
      // The header's mode and approval controls, wherever the header put them (the header row at
      // level 0, the sub-header row once `fitHeader` has moved them), and whether the header still
      // overflows after it settled.
      const header = pane.querySelector("header");
      const rectOf = (el) => {
        const r = el.getBoundingClientRect();
        return { l: r.left, t: r.top, r: r.right, b: r.bottom };
      };
      const control = (label) => {
        // The shared chip's button is labelled "<label>, <value>"; the radio group by the label alone.
        const el = pane.querySelector(`[aria-label="${label}"], [aria-label^="${label},"]`);
        if (!el) return null;
        const radio = el.getAttribute("role") === "radiogroup";
        const box = radio ? el : el.parentElement;
        const r = rectOf(box);
        return {
          kind: radio ? "radiogroup" : "chip",
          where: box.closest("header") ? "header" : "subheader",
          width: Math.round(r.r - r.l),
          pastPaneEdgePx: Math.round((visRight(box) - pr.right) * 10) / 10,
          minWidth: getComputedStyle(box).minWidth,
          truncated: el.scrollWidth > el.clientWidth + 1,
          rect: r,
        };
      };
      const mode = control(plant === "nomode" ? "planted missing" : "Agent mode");
      const approval = control("Approvals");
      const gaps = pane.querySelector('[aria-label="What this agent runs without asking"]');
      const subheader = header?.nextElementSibling?.classList.contains("flex-wrap") ? header.nextElementSibling : null;
      const overlap = (a, b) => Math.max(0, Math.min(a.r, b.r) - Math.max(a.l, b.l)) * Math.max(0, Math.min(a.b, b.b) - Math.max(a.t, b.t));
      const rects = [mode?.rect, approval?.rect, gaps && rectOf(gaps)].filter(Boolean);
      const collisions = [];
      for (let i = 0; i < rects.length; i++) for (let j = i + 1; j < rects.length; j++) if (overlap(rects[i], rects[j]) > 1) collisions.push(`${i}x${j}`);
      for (const c of [mode, approval]) if (c) delete c.rect;
      return {
        paneWidth: Math.round(pr.width),
        headerControls: header && {
          mode,
          approval,
          headerOverflow: header.scrollWidth > header.clientWidth + 1,
          subheaderOverflow: subheader ? subheader.scrollWidth > subheader.clientWidth + 1 : null,
          collisions,
        },
        pastPaneEdge: past,
        queuedRow: li ? { rowRightMinusPaneRight: Math.round(li.getBoundingClientRect().right - pr.right), childWidths: [...li.children].map((c) => `${Math.round(c.getBoundingClientRect().width)}:${(c.textContent || "").trim().slice(0, 14)}`) } : null,
        usage: u ? { shown: u.innerText.replace(/\s+/g, " "), totalsClipped: tot.scrollWidth > tot.clientWidth, totalsOverlapRingPx: ring ? Math.max(0, Math.round(Math.min(visRight(tot), tot.getBoundingClientRect().right) - ring.left)) : null } : null,
        textarea: ta ? { clientHeight: ta.clientHeight, scrollHeight: ta.scrollHeight, clipped: ta.scrollHeight > ta.clientHeight + 1 } : null,
        workingLine: wk ? [...wk.querySelectorAll("span")].filter((s) => s.textContent.length > 1).map((s) => `${s.textContent.slice(0, 24)} | ${s.scrollWidth > s.clientWidth ? "TRUNCATED" : "full"} | ${s.clientWidth}px`) : null,
      };
    });
  const host = document.querySelector("[data-pane-host]");
  if (!host) throw new Error("no [data-pane-host]: the panes did not render");
  // A chip floor far too big: the header must read worse than it does without the floor.
  if (plant === "bigfloor") document.head.appendChild(Object.assign(document.createElement("style"), { textContent: "span.min-w-14, span.min-w-18 { min-width: 600px !important; }" }));
  const out = { window: `${innerWidth}x${innerHeight}`, sessions: titles, panes: {} };
  for (const w of widths) {
    host.style.flex = "none";
    host.style.width = `${4 * w}px`;
    inject();
    if (plant === "overflow") {
      const s = document.getElementById("planted-overflow") ?? document.head.appendChild(Object.assign(document.createElement("style"), { id: "planted-overflow" }));
      s.textContent = 'ul[aria-label="Queued messages"] li { min-width: 700px !important; }';
    }
    await new Promise((r) => setTimeout(r, 800));
    out.panes[w] = measure();
    // The same panes with the chip's minimum width taken off, to compare what the floor changes.
    const floorOff = document.head.appendChild(Object.assign(document.createElement("style"), { textContent: "span.min-w-14, span.min-w-18 { min-width: 0 !important; }" }));
    await new Promise((r) => setTimeout(r, 800));
    (out.noFloor ??= {})[w] = measure();
    floorOff.remove();
  }
  // One pane on its own (no split): `fitHeader` leaves the header alone then, so the mode and
  // approval controls sit in the header row itself, where the chip's floor adds to what must fit.
  useStore.setState({ panes: [], activeCodeSessionId: null });
  useStore.getState().selectCodeSession(ids[1]);
  await new Promise((r) => setTimeout(r, 1500));
  for (const w of singleWidths) {
    host.style.flex = "none";
    host.style.width = `${w}px`;
    inject();
    await new Promise((r) => setTimeout(r, 800));
    (out.single ??= {})[w] = measure();
    const floorOff = document.head.appendChild(Object.assign(document.createElement("style"), { textContent: "span.min-w-14, span.min-w-18 { min-width: 0 !important; }" }));
    await new Promise((r) => setTimeout(r, 800));
    (out.singleNoFloor ??= {})[w] = measure();
    floorOff.remove();
  }
  return out;
}

const url = process.env.PB_URL ?? "http://localhost:1420";
const chromeBin = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const dir = mkdtempSync(join(tmpdir(), "pb-layout-measure-"));
const chrome = spawn(chromeBin, ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${dir}`, "--no-first-run", "--use-mock-keychain", "--password-store=basic", "--window-size=1056,753", "about:blank"], { stdio: "ignore" });
// A missing or unusable Chrome says so and leaves nothing behind (no unhandled ENOENT stack, no temp dir).
chrome.once("error", (e) => {
  console.error(`could not start Chrome at ${chromeBin}: ${e.code ?? e.message} (set CHROME to a Chrome or Chromium binary)`);
  cleanup();
  process.exit(2);
});
const cleanup = () => {
  chrome.kill();
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch {}
};
process.on("exit", cleanup);

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

await send("Page.enable");
// The fake backend loads its sessions after the page starts: wait until the panes' host exists.
let ready = false;
for (let i = 0; i < 120 && !ready; i++) {
  const r = await send("Runtime.evaluate", { expression: `!!document.body && document.body.innerText.length > 0`, returnByValue: true });
  ready = r.result?.result?.value === true;
  if (!ready) await sleep(250);
}
await sleep(1500);
const expr = `(${measureInPage.toString()})(${JSON.stringify(WIDTHS)}, ${JSON.stringify(SINGLE_WIDTHS)}, ${JSON.stringify(plant ?? null)})`;
const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
let result;
if (r.result?.exceptionDetails) {
  console.log(`MEASUREMENT ERROR: ${r.result.exceptionDetails.exception?.description ?? r.result.exceptionDetails.text}`);
  result = undefined;
} else {
  result = r.result?.result?.value;
}
ws.close();
cleanup();

if (result?.panes) {
  console.log(`window ${result.window}; panes left to right: ${(result.sessions ?? []).join(" | ")}`);
  for (const [w, panes] of Object.entries(result.panes))
    panes.forEach((p, i) => console.log(`pane=${p.paneWidth}px #${i + 1}; pastPaneEdge=${JSON.stringify(p.pastPaneEdge)}; queuedRow=${p.queuedRow ? `rowRight-paneRight=${p.queuedRow.rowRightMinusPaneRight}` : "none"}; usage=${p.usage ? JSON.stringify(p.usage.shown) : "none"} overlapRing=${p.usage?.totalsOverlapRingPx}px; textarea=${p.textarea ? `${p.textarea.clientHeight}/${p.textarea.scrollHeight} clipped=${p.textarea.clipped}` : "none"}; working=${JSON.stringify(p.workingLine)}`));
}
// The header's mode and approval controls, with the chip's floor (as shipped) and without it.
const ctl = (c) => (c ? `${c.kind}@${c.where} ${c.width}px min=${c.minWidth} past=${c.pastPaneEdgePx} trunc=${c.truncated}` : "none");
const headerLine = (tag, w, p, i) => {
  const h = p.headerControls;
  return `${tag} pane=${w}px #${i + 1}: ${h ? `mode[${ctl(h.mode)}] approval[${ctl(h.approval)}] headerOverflow=${h.headerOverflow} subheaderOverflow=${h.subheaderOverflow} collisions=${JSON.stringify(h.collisions)}` : "no header"}`;
};
if (result?.panes) {
  for (const [w, panes] of Object.entries(result.panes)) panes.forEach((p, i) => console.log(headerLine("header", w, p, i)));
  for (const [w, panes] of Object.entries(result.noFloor ?? {})) panes.forEach((p, i) => console.log(headerLine("header(no floor)", w, p, i)));
  for (const [w, panes] of Object.entries(result.single ?? {})) panes.forEach((p, i) => console.log(headerLine("single", w, p, i)));
  for (const [w, panes] of Object.entries(result.singleNoFloor ?? {})) panes.forEach((p, i) => console.log(headerLine("single(no floor)", w, p, i)));
}
const problems = layoutProblems(result);
if (plant) console.log(`planted failure "${plant}": ${problems.length} problem(s) found`);
try {
  assertLayout(result);
  console.log("layout measurement PASSED: 4 widths x 4 panes, every probe found, nothing past a pane edge");
} catch (e) {
  console.log(e.message);
  process.exit(1);
}
