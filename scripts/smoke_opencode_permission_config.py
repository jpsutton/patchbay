#!/usr/bin/env python3
"""pb-20261107-32: repeatable in-repo smoke for opencode permission config injection.

Honest scope: spawns fresh `opencode serve` children (NOT `opencode debug config`),
queries /api/config, and computes EFFECTIVE permission decisions in Python using
last-match-wins over opencode's reported sources/rule order. This does NOT exercise
the Rust probe/extractor/prepare() path (covered by Rust tests); it validates the
merge assumption and config shape on the installed opencode.

Asserts for each case:
  - defect planted (allows present, deny NOT re-emitted in winning CONTENT) => WRONG (allow)
  - fixed (allows + deny re-emitted last in winning CONTENT) => DENY
  - fixed CONTENT array: every re-emitted deny sits AFTER every injected allow (within-layer)
  - order-SWAP planted (deny before allows in CONTENT) => WRONG (allow)
Covers a PROJECT-LEVEL deny and a deny pattern re-emitted via the same mechanism.

Row wording (verbatim): "resolved decision not observable in opencode v2.1.3; guarded by
layer order + within-layer position + version tripwire."

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

VERIFIED_OPENCODE_VERSION = "2.1.3"
OPENCODE = os.environ.get("OPENCODE_BIN") or shutil.which("opencode")
if not OPENCODE:
    print("SKIP: opencode not found on PATH (set OPENCODE_BIN)")
    sys.exit(0)


def check_version_tripwire():
    out = subprocess.check_output([OPENCODE, "--version"], text=True).strip()
    # e.g. "opencode v2.1.3"
    ver = out.split()[-1].lstrip("v")
    if ver != VERIFIED_OPENCODE_VERSION:
        print(f"FAIL: opencode upgraded; re-verify permission merge semantics")
        print(f"  observed: {out!r} (parsed {ver!r}), verified-on: {VERIFIED_OPENCODE_VERSION!r}")
        sys.exit(1)
    print(f"version tripwire ok: {out} (verified-on {VERIFIED_OPENCODE_VERSION})")


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


def content_permissions(sources):
    for doc in sources:
        if doc.get("type") == "document" and not doc.get("path") and (doc.get("info") or {}).get("permissions"):
            return doc["info"]["permissions"]
    raise AssertionError(f"no injected CONTENT permissions doc found: {json.dumps(sources, indent=2)}")


def assert_denies_after_allows_in_content(sources, deny_rules):
    perms = content_permissions(sources)
    injected_allows = [(i, r) for i, r in enumerate(perms) if r.get("effect") == "allow" and r.get("action") in ("shell", "external_directory")]
    deny_idxs = []
    for dr in deny_rules:
        for i, r in enumerate(perms):
            if r.get("effect") == "deny" and r.get("action") == dr.get("action") and r.get("resource") == dr.get("resource"):
                deny_idxs.append((i, r))
    assert injected_allows and deny_idxs, f"missing allows/denies in CONTENT: {json.dumps(perms, indent=2)}"
    last_allow = max(i for i, _ in injected_allows)
    for i, r in deny_idxs:
        assert i > last_allow, f"deny at {i} not after every injected allow (last allow {last_allow}): {json.dumps(perms, indent=2)}"
    print(f"  within-layer: every re-emitted deny after every injected allow in CONTENT (last allow={last_allow}, denies={[i for i,_ in deny_idxs]})")


def run_case(name, deny_rule, deny_action, deny_resource_command):
    print(f"\n== {name} ==")
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        home = td / "home"
        project = td / "project"
        (home / ".config/opencode").mkdir(parents=True)
        project.mkdir()
        (home / ".config/opencode/opencode.jsonc").write_text('{"$schema":"https://opencode.ai/config.json"}\n')
        # Place deny in project config so opencode loads it as a project-layer source (realistic);
        # the fix must re-emit it in winning CONTENT regardless of source layer.
        (project / "opencode.json").write_text(json.dumps({"permissions": [deny_rule]}))

        allows = [
            {"action": "*", "resource": "*", "effect": "ask"},
            {"action": "shell", "resource": "*", "effect": "allow"},
            {"action": "external_directory", "resource": "*", "effect": "allow"},
        ]

        # Defect planted: allows in winning CONTENT, deny NOT re-emitted
        log = td / "defect.log"
        proc, port, pw = serve(project, home, json.dumps({"permissions": allows}), log)
        try:
            sources = api_config(port, pw)
            assert_decision(sources, deny_action, deny_resource_command, "allow",
                            "DEFECT PLANTED (no re-emit) resolves WRONG")
        finally:
            kill(proc)

        # Fixed: allows + deny re-emitted last in winning CONTENT
        log = td / "fixed.log"
        proc, port, pw = serve(project, home, json.dumps({"permissions": allows + [deny_rule]}), log)
        try:
            sources = api_config(port, pw)
            assert_decision(sources, deny_action, deny_resource_command, "deny",
                            "FIXED (deny re-emitted last) resolves DENY")
            assert_denies_after_allows_in_content(sources, [deny_rule])
        finally:
            kill(proc)

        # Order-SWAP planted (deny before allows in CONTENT) => WRONG (allow)
        log = td / "swap.log"
        proc, port, pw = serve(project, home, json.dumps({"permissions": [deny_rule] + allows}), log)
        try:
            sources = api_config(port, pw)
            assert_decision(sources, deny_action, deny_resource_command, "allow",
                            "ORDER-SWAP PLANTED (deny before allows in CONTENT) resolves WRONG")
        finally:
            kill(proc)
    print(f"OK: {name}")


def main():
    print(f"opencode: {OPENCODE} ({subprocess.check_output([OPENCODE, '--version'], text=True).strip()})")
    check_version_tripwire()
    run_case(
        "project-layer deny (re-emitted in winning CONTENT)",
        {"action": "shell", "resource": "echo PROJECT_DENY_SECRET*", "effect": "deny"},
        "shell",
        "echo PROJECT_DENY_SECRET payload",
    )
    run_case(
        "deny pattern (project-layer source, re-emitted in winning CONTENT)",
        {"action": "shell", "resource": "rm -rf /tmp/pb-smoke-deny*", "effect": "deny"},
        "shell",
        "rm -rf /tmp/pb-smoke-deny x",
    )
    print("\nALL SMOKE ASSERTIONS PASSED (effective decisions, defect=>WRONG, fixed=>DENY, order-swap=>WRONG,")
    print("within-layer denies-after-allows, project-layer case, version tripwire; python last-match-wins,")
    print("does NOT exercise Rust probe/extractor)")


if __name__ == "__main__":
    main()
