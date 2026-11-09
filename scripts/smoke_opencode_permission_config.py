#!/usr/bin/env python3
"""pb-20261107-32: repeatable in-repo smoke for opencode permission config injection.

Spawns fresh `opencode serve` children (NOT `opencode debug config`) and queries
/api/config. Asserts EFFECTIVE permission decisions (last-match-wins over
opencode's reported sources in order) for deny patterns in BOTH states:
  - defect planted (allows present, deny NOT re-emitted in winning CONTENT) => WRONG (allow)
  - fixed (allows + deny re-emitted last in winning CONTENT) => DENY
Covers a global-style deny pattern and a PROJECT-LEVEL deny (verifier finding).

Usage: python3 scripts/smoke_opencode_permission_config.py
Requires: opencode on PATH (or OPENCODE_BIN).
"""
from __future__ import annotations

import base64
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from fnmatch import fnmatch
from pathlib import Path

OPENCODE = os.environ.get("OPENCODE_BIN") or shutil.which("opencode")
if not OPENCODE:
    print("SKIP: opencode not found on PATH (set OPENCODE_BIN)")
    sys.exit(0)


def wait_ready(log_path: Path, timeout: float = 15.0) -> tuple[int, str]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if log_path.exists():
            text = log_path.read_text(errors="replace")
            for line in text.splitlines():
                if "listening on http://127.0.0.1:" in line:
                    port = int(line.rsplit(":", 1)[1].strip())
                elif "server listening on http://127.0.0.1:" in line:
                    port = int(line.rsplit(":", 1)[1].strip())
                else:
                    continue
                pw = None
                for l2 in text.splitlines():
                    if "server password " in l2:
                        pw = l2.split("server password ", 1)[1].strip()
                if pw:
                    return port, pw
        time.sleep(0.2)
    raise RuntimeError(f"opencode serve not ready: {log_path.read_text(errors='replace')[-800:]}")


def serve(cwd: Path, home: Path, config_content: str | None, log_path: Path):
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    if config_content is None:
        env.pop("OPENCODE_CONFIG_CONTENT", None)
    else:
        env["OPENCODE_CONFIG_CONTENT"] = config_content
    proc = subprocess.Popen(
        [OPENCODE, "serve", "--port", "0", "--hostname", "127.0.0.1"],
        cwd=str(cwd),
        env=env,
        stdout=open(log_path, "wb"),
        stderr=subprocess.STDOUT,
    )
    port, pw = wait_ready(log_path)
    return proc, port, pw


def api_config(port: int, pw: str):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/config",
        headers={"Authorization": "Basic " + base64.b64encode(f"opencode:{pw}".encode()).decode()},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


def kill(proc):
    try:
        proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=5)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass


def merged_permissions(sources):
    # Effective order follows opencode's reported source order; within each source,
    # permissions array order is preserved (observed/pinned). Last matching rule wins.
    rules = []
    for doc in sources:
        info = doc.get("info") or {}
        perms = info.get("permissions") or []
        if isinstance(perms, list):
            rules.extend(perms)
    return rules


def effective_decision(rules, action: str, resource: str) -> str:
    # Last matching rule wins. Resource match: exact, wildcard '*', or fnmatch pattern.
    decision = "ask"
    for r in rules:
        if r.get("action") not in (action, "*"):
            continue
        pat = r.get("resource", "*")
        if pat == "*" or pat == resource or fnmatch(resource, pat):
            decision = r.get("effect", decision)
    return decision


def assert_decision(sources, action, resource, expected, label):
    rules = merged_permissions(sources)
    got = effective_decision(rules, action, resource)
    print(f"  {label}: effective {action} {resource!r} => {got} (expected {expected})")
    assert got == expected, f"{label}: expected {expected}, got {got}\nrules={json.dumps(rules, indent=2)}"


def run_case(name, project_deny_rule, deny_action, deny_resource_command):
    print(f"\n== {name} ==")
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        home = td / "home"
        project = td / "project"
        (home / ".config/opencode").mkdir(parents=True)
        project.mkdir()
        (home / ".config/opencode/opencode.jsonc").write_text('{"$schema":"https://opencode.ai/config.json"}\n')
        if project_deny_rule:
            (project / "opencode.json").write_text(json.dumps({"permissions": [project_deny_rule]}))

        allows = [
            {"action": "*", "resource": "*", "effect": "ask"},
            {"action": "shell", "resource": "*", "effect": "allow"},
            {"action": "external_directory", "resource": "*", "effect": "allow"},
        ]

        # Defect planted: allows in winning CONTENT, deny NOT re-emitted
        defect_content = json.dumps({"permissions": allows})
        log = td / "defect.log"
        proc, port, pw = serve(project, home, defect_content, log)
        try:
            sources = api_config(port, pw)
            assert_decision(sources, deny_action, deny_resource_command, "allow",
                            "DEFECT PLANTED (no re-emit) resolves WRONG")
        finally:
            kill(proc)

        # Fixed: allows + deny re-emitted last in winning CONTENT
        fixed_content = json.dumps({"permissions": allows + [project_deny_rule]})
        log = td / "fixed.log"
        proc, port, pw = serve(project, home, fixed_content, log)
        try:
            sources = api_config(port, pw)
            assert_decision(sources, deny_action, deny_resource_command, "deny",
                            "FIXED (deny re-emitted last) resolves DENY")
        finally:
            kill(proc)
    print(f"OK: {name}")


def main():
    print(f"opencode: {OPENCODE} ({subprocess.check_output([OPENCODE, '--version'], text=True).strip()})")
    run_case(
        "project-level deny",
        {"action": "shell", "resource": "echo PROJECT_DENY_SECRET*", "effect": "deny"},
        "shell",
        "echo PROJECT_DENY_SECRET payload",
    )
    run_case(
        "global-pattern deny in winning content",
        {"action": "shell", "resource": "rm -rf /tmp/pb-smoke-deny*", "effect": "deny"},
        "shell",
        "rm -rf /tmp/pb-smoke-deny x",
    )
    print("\nALL SMOKE ASSERTIONS PASSED (effective decisions, defect=>WRONG, fixed=>DENY, project deny included)")


if __name__ == "__main__":
    main()
