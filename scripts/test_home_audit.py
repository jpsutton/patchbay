#!/usr/bin/env python3
"""Fail loudly when the test suite writes into $HOME.

Runs a command (default: the src-tauri lib tests) with HOME pointed at a fresh empty directory,
lists the directory before and after (path, size, sha256), and exits 1 printing every path that
appeared, changed or disappeared. Children that tests spawn (shells, agent CLIs, version probes)
write their own state under $HOME; this catches writers nobody has found yet.

    python3 scripts/test_home_audit.py                       # the lib suite
    python3 scripts/test_home_audit.py -- sh -c 'touch "$HOME/x"'   # prove the check itself fails
    python3 scripts/test_home_audit.py --cwd DIR -- ./some-test-binary   # audit any command

CARGO_HOME / RUSTUP_HOME keep pointing at the real toolchain so the build still works.
Nothing under the real home is read, written or deleted.
"""
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CMD = ["cargo", "test", "--lib", "--locked", "--features", "ipc-test", "-j", "4"]
DROPPED_ENV = ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "CLAUDE_CONFIG_DIR", "GROK_HOME")


def snapshot(root: Path) -> dict[str, str]:
    """Relative path -> 'dir' or 'size:sha256' for everything under `root`."""
    out: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        rel = str(path.relative_to(root))
        if path.is_symlink():
            out[rel] = "link:" + os.readlink(path)
        elif path.is_dir():
            out[rel] = "dir"
        else:
            try:
                data = path.read_bytes()
                out[rel] = f"{len(data)}:{hashlib.sha256(data).hexdigest()}"
            except OSError as e:
                out[rel] = f"unreadable:{e.errno}"
    return out


def diff(before: dict[str, str], after: dict[str, str]) -> list[str]:
    lines = [f"NEW      {p}" for p in sorted(set(after) - set(before))]
    lines += [f"CHANGED  {p}" for p in sorted(p for p in set(before) & set(after) if before[p] != after[p])]
    lines += [f"REMOVED  {p}" for p in sorted(set(before) - set(after))]
    return lines


def main(argv: list[str]) -> int:
    head = argv[: argv.index("--")] if "--" in argv else argv
    cmd = argv[argv.index("--") + 1:] if "--" in argv else DEFAULT_CMD
    cwd_arg = head[head.index("--cwd") + 1] if "--cwd" in head else None
    real_home = Path.home()
    home = Path(tempfile.mkdtemp(prefix="pb-home-audit-"))
    env = {k: v for k, v in os.environ.items() if k not in DROPPED_ENV}
    env["HOME"] = str(home)
    env.setdefault("CARGO_HOME", str(real_home / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(real_home / ".rustup"))
    cwd = Path(cwd_arg) if cwd_arg else ROOT / "src-tauri" if cmd is DEFAULT_CMD else ROOT
    try:
        before = snapshot(home)
        print(f"HOME={home}\nbefore: {len(before)} entries")
        code = subprocess.run(cmd, cwd=cwd, env=env).returncode
        after = snapshot(home)
        print(f"after:  {len(after)} entries")
        changes = diff(before, after)
        if changes:
            print("\nFAIL: the run touched $HOME. Paths:")
            print("\n".join("  " + c for c in changes))
            return 1
        if code != 0:
            print(f"\nthe command failed (exit {code}); HOME untouched")
            return code
        print("\nOK: $HOME byte-identical before and after")
        return 0
    finally:
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
