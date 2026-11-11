// Live proof of Codex's sandbox under Patchbay's approval modes (pb-20261110-15), against the real
// adapter Patchbay runs. Run by hand: it starts codex-acp, uses YOUR `codex login`, and each case is
// one small Codex turn. It writes only under the scratch folder, which must NOT be under /tmp,
// /private/tmp or $TMPDIR (the sandbox lets Codex write there, which would confound case B).
//
//   node scripts/e2e-codex-approval.mjs <scratch-dir> <case> [wire-log]
//   case: A | B | C | D | E | F | G | H | I | J   (F, G and I only report; see CASES)
//
// Prints one WIRE line per request_permission and the file checks. Exit 1 when a case's claim fails.
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";

const ADAPTER = "@agentclientprotocol/codex-acp@2.2.2";
const [scratchArg, which, wirePath] = process.argv.slice(2);
if (!scratchArg || !which) throw new Error("usage: e2e-codex-approval.mjs <scratch-dir> <A|B|C|D|E> [wire-log]");
const scratch = resolve(scratchArg);
const project = join(scratch, "project");
const outside = join(scratch, "outside");
for (const d of [project, outside]) mkdirSync(d, { recursive: true });
const real = { project: realpathSync(project), outside: realpathSync(outside), tmpdir: realpathSync(tmpdir()), TMPDIR: process.env.TMPDIR ?? "(unset)" };
console.log("REALPATH", JSON.stringify(real));
for (const root of ["/tmp", "/private/tmp", real.tmpdir]) {
  if (real.project.startsWith(root + "/") || real.outside.startsWith(root + "/")) throw new Error(`scratch is under ${root}: case B would be confounded`);
}

// Each case: the mode Codex starts in (what Patchbay maps Approve all / Ask to), what to write, and
// how permission requests are answered. D switches mid-session, as Patchbay does on Approve all -> Ask.
const CASES = {
  A: { mode: "workspace-write", file: join(real.project, "a.txt"), answer: "allow", expect: { asked: false, exists: true } },
  B: { mode: "workspace-write", file: join(real.outside, "b.txt"), answer: "allow", expect: { asked: true, exists: true } },
  C: { mode: "read-only", file: join(real.project, "c.txt"), answer: "deny", expect: { asked: true, exists: false } },
  D: { mode: "workspace-write", switchTo: "read-only", file: join(real.project, "d.txt"), answer: "deny", expect: { asked: true, exists: false } },
  E: { mode: "read-only", file: join(real.outside, "e.txt"), answer: "deny", expect: { asked: true, exists: false } },
  // The row's "before": Ask's sandbox with every request approved (what Approve all did). No claim; it reports.
  F: { mode: "read-only", file: join(real.project, "f.txt"), answer: "allow", expect: null },
  // The same, asked the way a user would (no hint to escalate): what Approve all used to do, and what it does now.
  G: { mode: "read-only", file: join(real.project, "g.txt"), answer: "allow", natural: true, expect: null },
  // Through Codex's file-edit tool instead of the shell (what a user's "edit this file" usually becomes).
  I: { mode: "read-only", file: join(real.project, "i.txt"), answer: "allow", natural: "patch", expect: null },
  J: { mode: "workspace-write", file: join(real.project, "j.txt"), answer: "allow", natural: "patch", expect: { exists: true } },
  H: { mode: "workspace-write", file: join(real.project, "h.txt"), answer: "allow", natural: true, expect: { exists: true } },
};
const c = CASES[which];
if (!c) throw new Error(`unknown case ${which}`);
rmSync(c.file, { force: true });

const wire = (dir, obj) => {
  const line = JSON.stringify(obj);
  if (wirePath) appendFileSync(wirePath, `${new Date().toISOString()} ${dir} ${line}\n`);
};
const env = { ...process.env, INITIAL_AGENT_MODE: c.mode };
const child = spawn("npx", ["--yes", ADAPTER], { env, cwd: real.project, stdio: ["pipe", "pipe", "inherit"] });
let nextId = 1;
const waiting = new Map();
let buf = "";
const asked = [];
let phase = "first";
const send = (obj) => {
  wire("->", obj);
  child.stdin.write(JSON.stringify(obj) + "\n");
};
const request = (method, params) =>
  new Promise((res, rej) => {
    const id = nextId++;
    waiting.set(id, { res, rej });
    send({ jsonrpc: "2.0", id, method, params });
  });
child.stdout.on("data", (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let m;
    try {
      m = JSON.parse(line);
    } catch {
      continue;
    }
    wire("<-", m);
    if (m.id !== undefined && !m.method) {
      const w = waiting.get(m.id);
      waiting.delete(m.id);
      if (w) (m.error ? w.rej(Object.assign(new Error(m.error.message), { rpc: m.error })) : w.res(m.result));
    } else if (m.method === "session/request_permission") {
      const opts = m.params.options ?? [];
      const pick = c.answer === "allow" ? opts.find((o) => o.kind === "allow_once") : opts.find((o) => o.kind === "reject_once") ?? opts.find((o) => o.kind.startsWith("reject"));
      asked.push({ phase, title: m.params.toolCall?.title, kind: m.params.toolCall?.kind, answered: pick?.kind });
      console.log(`WIRE request_permission phase=${phase} tool=${JSON.stringify(m.params.toolCall?.title)} answered=${pick?.kind} (options: ${opts.map((o) => o.kind).join(",")})`);
      send({ jsonrpc: "2.0", id: m.id, result: { outcome: pick ? { outcome: "selected", optionId: pick.optionId } : { outcome: "cancelled" } } });
    } else if (m.method && m.id !== undefined) {
      send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "not supported by this proof client" } });
    }
  }
});

const prompt = (sessionId, text) => request("session/prompt", { sessionId, prompt: [{ type: "text", text }] });
const ask = (file) =>
  `Run exactly this one shell command and nothing else: printf proof > '${file}'. ` +
  `If it fails because the sandbox denies the write, run the same command again requesting escalated permissions so you may be asked. ` +
  `If you are refused, stop and say refused. Do not try any other way to create the file.`;

const timeout = setTimeout(() => {
  console.log("TIMEOUT");
  child.kill();
  process.exit(2);
}, 240_000);
try {
  const init = await request("initialize", { protocolVersion: 1, clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false } });
  console.log("ADAPTER", JSON.stringify(init.agentInfo ?? {}), "mode requested:", c.mode);
  const session = await request("session/new", { cwd: real.project, mcpServers: [] });
  const sid = session.sessionId;
  const modes = session.modes ?? {};
  console.log("MODE after session/new:", modes.currentModeId, "available:", (modes.availableModes ?? []).map((m) => m.id).join(","));
  if (modes.currentModeId !== c.mode) {
    await request("session/set_mode", { sessionId: sid, modeId: c.mode });
    console.log("MODE set to", c.mode);
  }
  if (c.switchTo) {
    const stop = await prompt(sid, "Say only: ready");
    console.log("warm-up stop:", stop.stopReason);
    await request("session/set_mode", { sessionId: sid, modeId: c.switchTo });
    phase = "after-switch";
    console.log("MODE switched mid-session to", c.switchTo);
  }
  const stop = await prompt(sid, c.natural === "patch" ? `Use your apply_patch file-editing tool (not a shell command) to add the new file ${c.file} containing the word proof.` : c.natural ? `Create the file ${c.file} containing the word proof.` : ask(c.file));
  console.log("TURN stopReason:", stop.stopReason);
} catch (e) {
  console.log("ERROR", e.message, JSON.stringify(e.rpc ?? {}));
}
clearTimeout(timeout);
child.kill();
const exists = existsSync(c.file);
const wasAsked = asked.some((a) => a.phase === (c.switchTo ? "after-switch" : "first"));
console.log(`CASE ${which}: file ${c.file} exists=${exists}${exists ? ` content=${JSON.stringify(readFileSync(c.file, "utf8"))}` : ""}; asked=${wasAsked} (expected ${c.expect ? JSON.stringify(c.expect) : "none: reports only"})`);
const ok = !c.expect || ((c.expect.asked === undefined || wasAsked === c.expect.asked) && exists === c.expect.exists);
console.log(ok ? `CASE ${which} OK` : `CASE ${which} FAILED`);
process.exit(ok ? 0 : 1);
