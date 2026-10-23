#!/usr/bin/env node
// Scripted checks of Patchbay Remote in the emulator, against the node `up` started. They read the
// phone's own DOM over the DevTools protocol, so a passing run means the screen showed it.
//
//   scenarios.mjs --seed FILE --src DIR [--only a,b]     (--list prints the names)
//
// A scenario that needs code the checked-out source does not have is SKIPPED, not failed, so the
// same suite runs on every branch. Exit 1 when any scenario FAILED; skipped ones are not a failure.
// `adb` picks the device from ANDROID_SERIAL. Needs Node 22 or newer.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { adb, attach, clickButton, until } from "./phone.mjs";

const APP = "dev.patchbay.remote";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const J = JSON.stringify;

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((a, v, i, all) => (v.startsWith("--") ? [...a, [v.slice(2), all[i + 1]?.startsWith("--") || all[i + 1] == null ? "1" : all[i + 1]]] : a), []),
);

/** True when `file` exists in the checked-out source and, if given, contains `text`. */
const source = (file, text) => {
  const p = join(args.src ?? ".", file);
  return existsSync(p) && (text == null || readFileSync(p, "utf8").includes(text));
};

// ---- things done in the page ----

const click = (page, sel) => page.evalJs(`(() => { const e = document.querySelector(${J(sel)}); if (!e || e.disabled) return false; e.click(); return true; })()`);
const clickText = (page, text) =>
  page.evalJs(`(() => { const want = ${J(text)}; const e = [...document.querySelectorAll('*')].find((x) => x.children.length === 0 && x.textContent.trim() === want); if (!e) return false; e.click(); return true; })()`);
const setComposer = (page, value) =>
  page.evalJs(`(() => { const el = document.querySelector('textarea'); if (!el) return false; Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(el, ${J(value)}); el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
const waitText = (page, what, re, ms = 30_000) => until(what, ms, async () => re.test(await page.text()));
const waitFor = (page, what, sel, ms = 30_000) => until(what, ms, () => page.evalJs(`!!document.querySelector(${J(sel)})`));
const labels = (page) =>
  page.evalJs(`[...document.querySelectorAll('[aria-label^="Reorder "]')].map((e) => e.getAttribute('aria-label').slice(8))`);

async function main(page) {
  await until("the Chats and Code lists", 60_000, async () => /\bChats\b/.test(await page.text()) && /\bCode\b/.test(await page.text()));
  return page;
}

/** Back to the lists, then into the seeded session. */
async function openSession(ctx, title = ctx.seed.session.title) {
  const { page } = ctx;
  for (let i = 0; i < 4 && (await page.evalJs(`!!document.querySelector('[aria-label="Back"]')`)); i++) {
    await click(page, '[aria-label="Back"]');
    await sleep(500);
  }
  await until("the Code tab", 10_000, () => page.evalJs(clickButton("Code", false)));
  await until(`"${title}" in the list`, 15_000, () => clickText(page, title));
  await waitFor(page, "the composer", "textarea", 20_000);
}

async function send(ctx, text) {
  await until("the composer", 10_000, () => setComposer(ctx.page, text));
  await until("Send", 5_000, () => click(ctx.page, '[aria-label="Send"]'));
}

/** One finger down, a slow drag, one finger up, in the page's own pixels. */
async function drag(page, from, to) {
  const touch = (type, p) => page.send("Input.dispatchTouchEvent", { type, touchPoints: p ? [{ x: p.x, y: p.y }] : [] });
  await touch("touchStart", from);
  await sleep(600);
  for (let i = 1; i <= 14; i++) {
    await touch("touchMove", { x: from.x + ((to.x - from.x) * i) / 14, y: from.y + ((to.y - from.y) * i) / 14 });
    await sleep(40);
  }
  await sleep(300);
  await touch("touchEnd");
}

// ---- the scenarios, in the order they build on each other ----

const scenarios = [
  {
    name: "send-and-ask",
    note: "a Code turn from the phone reaches the host and its permission card shows",
    async run(ctx) {
      await openSession(ctx);
      await send(ctx, "!edit");
      await waitText(ctx.page, "the permission card", /Permission required/);
    },
  },
  {
    name: "message-times",
    needs: () => source("src/components/MessageTime.tsx"),
    note: "messages carry a time on the phone",
    async run(ctx) {
      await waitText(ctx.page, "a message time", /just now|\b\d{1,2}:\d{2}\s?(AM|PM)?\b/i, 10_000);
    },
  },
  {
    name: "edit-after-stop",
    needs: () => source("src/code/CodeView.tsx", "amendAfterStop"),
    note: "Stop on the phone reopens the last message for editing, with Resend",
    async run(ctx) {
      await until("Stop", 10_000, () => click(ctx.page, '[aria-label="Stop"]'));
      await until("Resend", 15_000, () =>
        ctx.page.evalJs(`[...document.querySelectorAll('button')].some((b) => b.textContent.trim() === 'Resend')`),
      );
      const v = await ctx.page.evalJs(`document.querySelector('textarea')?.value ?? ''`);
      if (!v.includes("!edit")) throw new Error(`the edit box holds "${v}", not the stopped message`);
      await until("Cancel", 5_000, () => ctx.page.evalJs(clickButton("Cancel", true)));
    },
  },
  {
    name: "diff-sheet",
    needs: () => source("src/code/CodeView.tsx", "lines added"),
    note: "an allowed edit shows in the phone's Changes sheet with its line counts",
    async run(ctx) {
      await send(ctx, "!edit");
      await waitText(ctx.page, "the permission card", /Permission required/);
      await until("Allow", 10_000, () => ctx.page.evalJs(clickButton("Allow", true)));
      await waitFor(ctx.page, "the Diff chip", '[aria-label^="Diff,"]', 30_000);
      await click(ctx.page, '[aria-label^="Diff,"]');
      await waitText(ctx.page, "the Changes sheet", /Changes[\s\S]*patchbay-test\.txt/);
      await until("Close changes", 5_000, () => click(ctx.page, '[aria-label="Close changes"]'));
    },
  },
  {
    name: "session-info",
    needs: () => source("src/code/SessionInfoDialog.tsx"),
    note: "the info dialog shows both session ids; with host info, the host's version and OS",
    async run(ctx) {
      await until("the Session info button", 10_000, () => click(ctx.page, '[aria-label^="Session info"]'));
      await waitText(ctx.page, "the dialog", /Session info[\s\S]*Patchbay session ID/);
      const t = await ctx.page.text();
      const ids = t.match(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g) ?? [];
      if (ids.length < 2) throw new Error(`expected a Patchbay and an agent session id, saw ${ids.length}`);
      if (source("src-tauri/src/lib.rs", "host_info")) {
        await waitText(ctx.page, "the host's Patchbay version and OS", /Patchbay\s+\d+\.\d+\.\d+\s*·\s*(macOS|Linux|Windows)/, 20_000);
      }
      await until("Close", 5_000, () => click(ctx.page, '[aria-label="Close"]'));
    },
  },
  {
    name: "group-and-reorder",
    needs: () => source("src/remote/CodeSessionList.tsx", "Reorder"),
    note: "sessions group by project, a group drags to a new place, and the order survives a restart",
    async run(ctx) {
      const { page } = ctx;
      for (let i = 0; i < 4 && (await page.evalJs(`!!document.querySelector('[aria-label="Back"]')`)); i++) {
        await click(page, '[aria-label="Back"]');
        await sleep(500);
      }
      await until("Settings", 10_000, () => click(page, '[aria-label="Settings"]'));
      await until("the grouping switch", 10_000, () =>
        page.evalJs(`(() => { const l = [...document.querySelectorAll('label')].find((x) => x.textContent.includes('Group code sessions by project')); const i = l?.querySelector('input'); if (!i) return false; if (!i.checked) i.click(); return true; })()`),
      );
      await until("Close", 5_000, () => click(page, '[aria-label="Close"]'));
      await until("the Code tab", 10_000, () => page.evalJs(clickButton("Code", false)));
      await until("three or more project groups", 20_000, async () => (await labels(page)).length >= 3);
      const before = await labels(page);
      const last = before[before.length - 1];
      const at = (name) =>
        page.evalJs(`(() => { const e = [...document.querySelectorAll('[aria-label^="Reorder "]')].find((x) => x.getAttribute('aria-label') === ${J(`Reorder ${name}`)}); if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
      const from = await at(last);
      const to = await at(before[0]);
      if (!from || !to) throw new Error("could not find the group drag handles");
      await drag(page, from, { x: to.x, y: to.y - 20 });
      await until("the group to move to the top", 10_000, async () => (await labels(page))[0] === last);
      const after = await labels(page);
      // The order stays on the phone: stop the app, start it again, look again. The WebView writes its
      // storage a moment after the drag, so a kill in the same second would lose the order, as it would for a user.
      await sleep(4_000);
      ctx.release();
      adb("shell", "am", "force-stop", APP);
      adb("shell", "am", "start", "-n", `${APP}/.MainActivity`);
      ctx.page = await ctx.reattach();
      await until("the Code tab", 30_000, () => ctx.page.evalJs(clickButton("Code", false)));
      await until("the groups again", 20_000, async () => (await labels(ctx.page)).length === after.length);
      const kept = await labels(ctx.page);
      if (kept.join("|") !== after.join("|")) throw new Error(`the order was ${after.join(", ")}, after a restart ${kept.join(", ")}`);
    },
  },
];

async function run() {
  if (args.list) return scenarios.forEach((s) => console.log(`${s.name}\t${s.note}`));
  if (!args.seed) throw new Error("--seed is required");
  const seed = JSON.parse(readFileSync(args.seed, "utf8"));
  const only = args.only ? new Set(args.only.split(",")) : null;
  let page = await main(await attach());
  const ctx = {
    seed,
    get page() {
      return page;
    },
    set page(p) {
      page = p;
    },
    release: () => page.done(),
    reattach: async () => main(await attach()),
  };
  const results = [];
  try {
    for (const s of scenarios) {
      if (only && !only.has(s.name)) continue;
      if (s.needs && !s.needs()) {
        console.log(`SKIP ${s.name} (this checkout does not have it)`);
        results.push("skip");
        continue;
      }
      const t = Date.now();
      try {
        await s.run(ctx);
        console.log(`ok   ${s.name} (${Math.round((Date.now() - t) / 1000)}s)`);
        results.push("ok");
      } catch (e) {
        console.log(`FAIL ${s.name}: ${e.message}`);
        results.push("fail");
      }
    }
  } finally {
    page.done();
  }
  const n = (k) => results.filter((r) => r === k).length;
  console.log(`scenarios: ${n("ok")} passed, ${n("fail")} failed, ${n("skip")} skipped`);
  if (n("fail")) process.exit(1);
}

run().catch((e) => {
  console.error(`scenarios.mjs: ${e.message}`);
  process.exit(1);
});
