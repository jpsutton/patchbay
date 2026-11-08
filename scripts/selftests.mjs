#!/usr/bin/env node
// Run every selftest in the repo, naming the one that fails (pb-20261107-16).
// CI used to run three of them by hand; this is the one list, so a selftest added
// later runs without anyone editing a workflow. Zero matches is a failure: a runner
// that finds nothing must not report success.
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dirs = ["src", "scripts"];

const found = [];
const walk = (dir) => {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path);
    else if (/\.selftest\.(ts|mjs)$/.test(entry.name)) found.push(path);
  }
};
for (const dir of dirs) walk(join(root, dir));
found.sort();

if (found.length === 0) {
  console.error(`selftests: no *.selftest.ts or *.selftest.mjs under ${dirs.join(", ")} — refusing to pass while testing nothing`);
  process.exit(1);
}

const failed = [];
for (const file of found) {
  const rel = file.slice(root.length + 1);
  console.log(`selftest: ${rel}`);
  // --experimental-strip-types: .selftest.ts files run as TypeScript under Node 22+;
  // harmless for .mjs. Keep in sync with how the local pre-push gate runs them.
  const run = spawnSync(process.execPath, ["--experimental-strip-types", file], { stdio: "inherit", cwd: root });
  if (run.status !== 0) {
    failed.push(rel);
    console.error(`selftests: FAILED ${rel} (exit ${run.status})`);
  }
}

console.log(`selftests: ${found.length - failed.length}/${found.length} passed`);
if (failed.length > 0) {
  console.error(`selftests: failed: ${failed.join(", ")}`);
  process.exit(1);
}
