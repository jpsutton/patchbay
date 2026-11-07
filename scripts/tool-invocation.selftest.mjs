/**
 * Run: node scripts/tool-invocation.selftest.mjs
 *
 * The notices gate's Windows portability proof (pb-20261106-13): the original
 * fail-loud proof only ever ran on macOS and Linux, so the win32 branch of the
 * spawn seam is asserted here by construction — no Windows box needed — and
 * the POSIX branch is asserted by actually executing it on this machine.
 */
import { invocation, runSync } from './tool-invocation.mjs';

function assert(cond, msg) {
  if (!cond) throw new Error(msg);
}
function same(actual, expected, msg) {
  assert(
    JSON.stringify(actual) === JSON.stringify(expected),
    `${msg}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`,
  );
}

const args = ['licenses', 'list', '--prod', '--json'];

// POSIX: direct exec, byte-for-byte the pre-seam behaviour — no shell, args
// untouched. (macOS and Linux both take this branch; behaviour must not change.)
for (const platform of ['linux', 'darwin']) {
  same(
    invocation('pnpm', args, platform),
    { file: 'pnpm', args, options: {} },
    `${platform}: pnpm must be exec'd directly, without a shell`,
  );
}

// win32: cmd.exe /d /s /c "<tool> <args…>" with verbatim arguments — the
// .cmd-capable invocation Node's own exec() builds, which is what makes
// corepack's pnpm.cmd launchable at all (CreateProcess cannot run it: ENOENT).
same(
  invocation('pnpm', args, 'win32', 'C:\\Windows\\System32\\cmd.exe'),
  {
    file: 'C:\\Windows\\System32\\cmd.exe',
    args: ['/d', '/s', '/c', '"pnpm licenses list --prod --json"'],
    options: { windowsVerbatimArguments: true },
  },
  'win32: pnpm must run through cmd.exe with the whole command as one quoted string',
);

// Default command shell: whatever ComSpec says, else cmd.exe. Asserted by
// suffix, not equality, so the selftest also passes on a real Windows box
// (where ComSpec is an absolute path).
assert(
  /cmd(\.exe)?$/i.test(invocation('pnpm', args, 'win32').file),
  'win32: default shell must be cmd.exe',
);

// A .cmd shim name reaches cmd.exe verbatim, which then resolves it via
// PATH/PATHEXT — the exact shape that used to die ENOENT.
const shim = invocation('pnpm.cmd', ['--version'], 'win32', 'cmd.exe');
assert(shim.args[3] === '"pnpm.cmd --version"', 'win32: a .cmd tool name is passed to cmd.exe');
assert(
  !('shell' in shim.options),
  'win32: no shell option — args alongside shell:true is DEP0190 (deprecated, warned since Node 22.15)',
);

// No args: the bare command is still wrapped for cmd.exe /c.
same(
  invocation('tool', [], 'win32', 'cmd.exe').args,
  ['/d', '/s', '/c', '"tool"'],
  'win32: argless command is quoted for cmd.exe /c',
);

// The seam really executes on this platform: POSIX runs the direct exec path
// (on a Windows box this runs the cmd.exe path for real).
const version = runSync('node', ['--version'], { encoding: 'utf8' });
assert(/^v\d+\./.test(version.trim()), `runSync executed node, got ${JSON.stringify(version)}`);

// Caller options must survive the seam (encoding above would have produced a
// Buffer, not a string; a dropped cwd must make the spawn fail loudly).
let cwdRejected = false;
try {
  runSync('node', ['-e', ''], { cwd: '__tool_invocation_selftest_missing_dir__' });
} catch (err) {
  cwdRejected = err.code === 'ENOENT';
}
assert(cwdRejected, 'runSync must pass caller options (cwd) through to child_process');

console.log('tool-invocation.selftest: ok');
