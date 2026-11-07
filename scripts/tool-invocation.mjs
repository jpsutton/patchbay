// How third-party-notices.mjs launches its external tools (pnpm, cargo) so the
// notices gate runs on every platform Patchbay develops on.
//
// Direct spawn (no shell) is what macOS/Linux want: exec the binary found on
// PATH, nothing in between. On Windows the npm/pnpm shims are .cmd batch files,
// which "are not executable on their own without a terminal, and therefore
// cannot be launched using child_process.execFile()" (Node docs, "Spawning .bat
// and .cmd files on Windows"), so a corepack-only box died ENOENT on
// spawnSync("pnpm", …) and the gate reported "Push aborted" although nothing
// was stale (pb-20261106-13).
//
// The win32 branch builds the invocation Node's own exec() builds internally
// for this case — `cmd.exe /d /s /c "<tool> <args…>"` with verbatim arguments
// (see Node's lib/child_process.js normalizeSpawnArguments) — rather than
// shell: true, because passing args alongside shell:true has been deprecated
// since Node 22.15/23.11 (DEP0190: args are concatenated, not escaped). The
// shape is identical to shell:true's, minus the deprecation; cmd.exe resolves
// the tool through PATH/PATHEXT, so pnpm.cmd, pnpm.exe and cargo.exe all work
// unchanged.
//
// Arguments are fixed literals at both call sites; they are joined into one
// command string on Windows, so keep them free of spaces and cmd
// metacharacters (asserted by the selftest's exact-shape expectations).

import { execFileSync } from 'node:child_process';

/**
 * The (file, args, options) triple execFileSync needs to run `file args…` on
 * `platform`. Pure: the platform and the command shell are parameters, so the
 * win32 shape can be asserted without a Windows box.
 *
 * POSIX  → direct exec, exactly as before the seam existed (no shell, args untouched).
 * win32  → cmd.exe /d /s /c "<file> <args…>", windowsVerbatimArguments.
 */
export function invocation(file, args, platform = process.platform, comspec = process.env.ComSpec ?? 'cmd.exe') {
  if (platform !== 'win32') return { file, args, options: {} };
  const command = args.length > 0 ? `${file} ${args.join(' ')}` : file;
  return {
    file: comspec,
    args: ['/d', '/s', '/c', `"${command}"`],
    options: { windowsVerbatimArguments: true },
  };
}

/** execFileSync through the portability seam: direct on POSIX, cmd.exe-routed on Windows. */
export function runSync(file, args, options) {
  const { file: shell, args: shellArgs, options: platformOptions } = invocation(file, args);
  return execFileSync(shell, shellArgs, { ...options, ...platformOptions });
}
