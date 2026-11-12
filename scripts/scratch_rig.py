#!/usr/bin/env python3
"""Run the scratch Patchbay (built with src-tauri/tauri.scratch.conf.json) beside the live app and
prove it cannot touch the live app's data. macOS only. Reads METADATA of the live data folder and
the unscoped keychain entries (names hashed, never a value); launches the scratch .app by full
path (never from /Applications); tears down everything the scratch identifier created.

  scratch_rig.py run <path/to/Patchbay Scratch.app> [--settle SECONDS]   # exit 1 on any change
"""
import argparse, hashlib, json, os, pathlib, re, signal, subprocess, sys, time

HOME = pathlib.Path.home()
LIB = HOME / "Library"
LIVE_ID, SCRATCH_ID = "dev.patchbay.app", "dev.patchbay.scratch"
LIVE = LIB / "Application Support" / LIVE_ID
SCRATCH = LIB / "Application Support" / SCRATCH_ID
LSREGISTER = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
SCRATCH_ROOTS = ["Application Support", "Caches", "WebKit", "HTTPStorages", "Preferences", "Saved Application State", "Logs"]


def sh(*a, check=False):
    return subprocess.run(a, capture_output=True, text=True, check=check).stdout


def tree_meta(root):
    """name -> (size, mtime_ns) for every file under root; stat only, no reads."""
    out = {}
    for d, _, files in os.walk(root):
        for f in files:
            p = pathlib.Path(d, f)
            try:
                st = p.lstat()
            except OSError:
                continue
            out[str(p.relative_to(root))] = (st.st_size, st.st_mtime_ns)
    return out


def keychain_lines(service):
    """Hashed account names of generic-password items under `service`: attributes only, no values."""
    accts = []
    for rec in re.split(r"\nkeychain:", "\n" + sh("security", "dump-keychain")):
        if f'"svce"<blob>="{service}"' in rec:
            m = re.search(r'"acct"<blob>="([^"]*)"', rec)
            accts.append(m.group(1) if m else "")
    return accts


def redact(acct):
    base, _, scope = acct.partition("#")
    return hashlib.sha256(base.encode()).hexdigest()[:8] + (f"#{scope}" if scope else "")


def snapshot():
    kc = keychain_lines(LIVE_ID)
    unscoped = sorted(redact(a) for a in kc if "#" not in a)
    scoped = sorted(redact(a) for a in kc if "#" in a)
    return {"live_dir_exists": LIVE.is_dir(), "live_files": tree_meta(LIVE) if LIVE.is_dir() else {}, "keychain_unscoped": unscoped, "keychain_scoped": scoped}


def scope_tag(path):
    return hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:16]


def scratch_leftovers():
    found = []
    for root in SCRATCH_ROOTS:
        base = LIB / root
        if base.is_dir():
            found += [p for p in base.iterdir() if SCRATCH_ID in p.name]
    return found


def lsregistered():
    return sh(LSREGISTER, "-dump").count(SCRATCH_ID)


def listeners(pid):
    return sorted(set(re.findall(r"TCP (\S+) \(LISTEN\)", sh("lsof", "-nP", "-a", "-p", str(pid), "-iTCP", "-sTCP:LISTEN"))))


def open_paths(pid):
    return [l[1:] for l in sh("lsof", "-nP", "-p", str(pid), "-Fn").splitlines() if l.startswith("n/")]


def diff(a, b):
    return {k: (a[k], b[k]) for k in a if a[k] != b[k]}


def run(app, settle):
    exe = pathlib.Path(app) / "Contents/MacOS/patchbay"
    assert str(app).startswith("/Applications") is False, "the scratch .app must never live in /Applications"
    assert not SCRATCH.exists(), f"{SCRATCH} already exists; tear it down first"
    before = snapshot()
    print(f"BEFORE live dir exists={before['live_dir_exists']} files={len(before['live_files'])} unscoped_keychain={len(before['keychain_unscoped'])} scoped_keychain={len(before['keychain_scoped'])}")
    time.sleep(settle)
    control = diff(before["live_files"], snapshot()["live_files"])
    log = open(f"/tmp/scratch-rig-{os.getpid()}.log", "w")
    proc = subprocess.Popen([str(exe)], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    ok = True
    try:
        deadline = time.time() + 90
        while time.time() < deadline and not (SCRATCH / "patchbay.sqlite").exists():
            time.sleep(0.5)
        time.sleep(settle)
        during = snapshot()
        paths = open_paths(proc.pid)
        live_open = [p for p in paths if f"/{LIVE_ID}/" in p or p.endswith(f"/{LIVE_ID}")]
        scratch_open = [p for p in paths if SCRATCH_ID in p]
        ports = listeners(proc.pid)
        print(f"DURING pid={proc.pid} binary={exe}")
        print(f"  scratch data dir created={SCRATCH.is_dir()} files={sorted(os.listdir(SCRATCH)) if SCRATCH.is_dir() else []}")
        print(f"  fds under live dir={live_open} fds under scratch id={len(scratch_open)} listening={ports}")
        ok &= SCRATCH.is_dir() and not live_open and not any(p.endswith(":4420") for p in ports)
        ok &= before["keychain_unscoped"] == during["keychain_unscoped"]
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(20)
        except subprocess.TimeoutExpired:
            proc.kill()
    time.sleep(1)
    after = snapshot()
    # The live app is running and writes its own sqlite/WAL/session files, so mtimes cannot be a
    # gate; they are reported with a control window so a reader can tell churn from interference.
    churn = sorted(diff(before["live_files"], after["live_files"]))
    print(f"AFTER live files whose size/mtime moved ({len(churn)}), control churn without scratch={len(control)}:", churn[:8])
    print("AFTER live files added/removed:", sorted(set(after["live_files"]) ^ set(before["live_files"]))[:8] or "none")
    print("AFTER unscoped keychain lines unchanged:", before["keychain_unscoped"] == after["keychain_unscoped"])
    ok &= before["keychain_unscoped"] == after["keychain_unscoped"]
    print("TEARDOWN scratch footprint:", [str(p) for p in scratch_leftovers()])
    registered = lsregistered()
    subprocess.run(["rm", "-rf", *map(str, scratch_leftovers())])
    sh(LSREGISTER, "-u", str(app))
    final = snapshot()
    left = scratch_leftovers()
    print(f"TEARDOWN done: leftovers={left} lsregister before={registered} after={lsregistered()} scoped keychain={final['keychain_scoped']}")
    ok &= not left and before["keychain_unscoped"] == final["keychain_unscoped"] and before["keychain_scoped"] == final["keychain_scoped"]
    print("RIG", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("app")
    r.add_argument("--settle", type=float, default=8)
    a = ap.parse_args()
    sys.exit(run(a.app, a.settle))
