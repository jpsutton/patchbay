// Writes THIRD_PARTY_NOTICES.md: every Rust crate and production npm package
// that Patchbay builds into its apps (the desktop app and the phone app), with
// their license texts. Crates in the grok sidecar are listed by Grok Build itself
// (vendor/grok-build/THIRD-PARTY-NOTICES), which release bundles ship next to this file.
//
//   pnpm notices
//
// Rust crates come from Cargo.lock for every target platform, so one file
// covers the Linux, Windows, macOS and Android builds. Dev-dependencies are left out.
// Identical license texts are printed once, with the packages that use them.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const out = join(root, 'THIRD_PARTY_NOTICES.md');

const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense)([-._].*)?$/i;

function licenseFiles(dir) {
  if (!dir || !existsSync(dir)) return [];
  return readdirSync(dir)
    .filter((f) => LICENSE_FILE.test(f) && statSync(join(dir, f)).isFile())
    .sort()
    .map((f) => readFileSync(join(dir, f), 'utf8').replace(/\r\n/g, '\n').trim())
    .filter(Boolean);
}

// Rust: walk the resolve graph from each app (the desktop app, and the phone app in remote/)
// through normal and build edges.
function cratesOf(dir, rootName) {
  const meta = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--locked'], {
    cwd: join(root, dir),
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  }));
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const rootId = meta.packages.find((p) => p.name === rootName && p.source === null).id;
  const seen = new Set([rootId]);
  const queue = [rootId];
  while (queue.length) {
    for (const dep of nodes.get(queue.pop()).deps) {
      if (seen.has(dep.pkg) || dep.dep_kinds.every((k) => k.kind === 'dev')) continue;
      seen.add(dep.pkg);
      queue.push(dep.pkg);
    }
  }
  return [...seen].map((id) => byId.get(id));
}
const allCrates = new Map();
for (const p of [...cratesOf('src-tauri', 'patchbay'), ...cratesOf('remote/src-tauri', 'patchbay-remote')]) {
  allCrates.set(`${p.name}@${p.version}`, p);
}
const crates = [...allCrates.values()]
  // Path crates are Patchbay's own and Grok Build's (see NOTICE).
  .filter((p) => p.source !== null)
  .map((p) => ({
    name: p.name,
    version: p.version,
    license: p.license ?? (p.license_file ? 'see text below' : 'unknown'),
    authors: p.authors,
    texts: licenseFiles(dirname(p.manifest_path)),
  }));

// npm: production dependencies, as pnpm resolves them.
const npmByLicense = JSON.parse(execFileSync('pnpm', ['licenses', 'list', '--prod', '--json'], {
  cwd: root,
  encoding: 'utf8',
  maxBuffer: 64 * 1024 * 1024,
}));
const packages = [];
for (const [license, list] of Object.entries(npmByLicense)) {
  for (const pkg of list) {
    for (const [i, version] of pkg.versions.entries()) {
      const dir = pkg.paths[i] ?? pkg.paths[0];
      let declared = license;
      if (license === 'Unknown') {
        try {
          const m = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
          declared = (typeof m.license === 'string' ? m.license : m.license?.type) ?? 'see text below';
        } catch {
          declared = 'see text below';
        }
      }
      const author = typeof pkg.author === 'string' ? pkg.author : pkg.author?.name;
      packages.push({ name: pkg.name, version, license: declared, authors: author ? [author] : [], texts: licenseFiles(dir) });
    }
  }
}

const byName = (a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version, undefined, { numeric: true });
crates.sort(byName);
packages.sort(byName);

// One entry per distinct license text, keyed by its hash.
const texts = new Map();
function textRefs(pkg) {
  return pkg.texts.map((text) => {
    const key = createHash('sha256').update(text).digest('hex').slice(0, 12);
    if (!texts.has(key)) texts.set(key, { text, users: [] });
    texts.get(key).users.push(`${pkg.name}@${pkg.version}`);
    return `[${key}](#text-${key})`;
  });
}

// A package that ships no license file gets the standard text of its declared
// license, with its authors as the copyright holders.
function fallback(p) {
  const ids = [...new Set(p.license.split(/\s+(?:OR|AND|WITH)\s+|[()/]/).map((s) => s.trim()).filter(Boolean))];
  const links = ids.map((id) => `[${id}](https://spdx.org/licenses/${id}.html)`).join(', ');
  const by = p.authors?.length ? `; copyright ${p.authors.join(', ').replace(/\|/g, '\\|')}` : '';
  return `standard text: ${links}${by}`;
}

const table = (list) => [
  '| Package | Version | License | Texts |',
  '|---|---|---|---|',
  ...list.map((p) => {
    const refs = textRefs(p);
    return `| ${p.name} | ${p.version} | ${p.license.replace(/\|/g, '\\|')} | ${refs.join(' ') || fallback(p)} |`;
  }),
];

const lines = [
  '# Third-party notices',
  '',
  'Patchbay is licensed under the Apache License 2.0 (see LICENSE and NOTICE).',
  'It builds in the components below, each under its own license. Where a component offers a choice of licenses, Patchbay may use it under any of them.',
  '',
  'Generated by `pnpm notices`. Do not edit by hand.',
  '',
  '## Grok Build',
  '',
  '[Grok Build](https://github.com/xai-org/grok-build), Copyright 2023-2026 SpaceXAI, Apache-2.0, with the local changes in `patches/`.',
  'Patchbay links its sampler crates and ships its agent as the `grok` sidecar.',
  'The crates in the sidecar are listed in `vendor/grok-build/THIRD-PARTY-NOTICES`, which release bundles include as `GROK_BUILD_THIRD_PARTY_NOTICES`.',
  '',
  '## Rust crates',
  '',
  `${crates.length} crates, built for every platform Patchbay supports. Each is used unmodified.`,
  '',
  ...table(crates),
  '',
  '## JavaScript packages',
  '',
  `${packages.length} packages, bundled into the frontend. Each is used unmodified.`,
  '',
  ...table(packages),
  '',
  '## License texts',
  '',
];
for (const [key, { text, users }] of texts) {
  lines.push(`### <a id="text-${key}"></a>${key}`, '', `Used by: ${users.join(', ')}`, '');
  lines.push('```text', text.replace(/```/g, "'''"), '```', '');
}
writeFileSync(out, lines.join('\n'));
console.log(`wrote ${out}: ${crates.length} crates, ${packages.length} npm packages, ${texts.size} distinct license texts`);
