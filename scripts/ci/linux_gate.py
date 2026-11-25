#!/usr/bin/env python3
"""Run CI's `linux` job (.github/workflows/checks.yml) in a Linux container on this Mac.

    linux_gate.py --repo <path> [--dry-run] [--colima-profile patchbay-ci]

Called by the local pre-push gate; the caller decides whether the diff needs it (src-tauri/,
crates/, Cargo.lock, checks.yml). Every `run:` step of the repo's `linux` job executes in the
container, in order, so the steps cannot drift from CI; the apt packages come from the repo's
scripts/ci/deps-ubuntu.sh. An empty list of steps parsed from checks.yml is a FAIL, not a pass.
The repo is mounted read-only. Cargo registry and target live in named volumes (warm runs are
incremental). One of the shared cargo-slot build slots is held for the run. Docker is addressed
through DOCKER_CONTEXT (default colima-<profile>). The colima profile is started when stopped and
never stopped.

The last stdout line is `linux: PASS <secs>s` (exit 0), `linux: FAIL <secs>s` (exit 1) or
`linux: NOT RUN (<reason>) <secs>s` (exit 0, docker/colima unavailable). A NOT RUN log is NOT
merge evidence for a Rust diff.
"""
import argparse
import fcntl
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

WORKFLOW = ".github/workflows/checks.yml"
DEPS = "scripts/ci/deps-ubuntu.sh"
DOCKERFILE = Path(__file__).resolve().parent / "linux-gate.Dockerfile"
IMAGE = "patchbay-linux-gate"
VOLUMES = ("patchbay-linux-cargo", "patchbay-linux-target")


def apt_packages(text):
    m = re.search(r"^packages=\(\n(.*?)^\)", text, re.S | re.M)
    return m.group(1).split() if m else []


def linux_steps(text):
    """[(working-directory, script)] for each `run:` step of the `linux` job. No YAML library needed.
    Empty when the job is missing or has no `cargo test` (parsing broke)."""
    m = re.search(r"^  linux:\n(.*?)(?=^  \S|\Z)", text, re.S | re.M)
    steps, wd, lines, i = [], ".", (m.group(1).splitlines() if m else []), 0
    while i < len(lines):
        line = lines[i]
        if re.match(r"\s{6}-\s", line):
            wd = "."
        if w := re.search(r"working-directory:\s*(\S+)", line):
            wd = w.group(1)
        if r := re.match(r"(\s*)(?:- )?run:\s*(.*)$", line):
            if r.group(2) in ("|", ">", "|-"):
                ind = len(r.group(1)) + 2
                body = []
                while i + 1 < len(lines) and (not lines[i + 1].strip() or len(lines[i + 1]) - len(lines[i + 1].lstrip()) >= ind):
                    i += 1
                    body.append(lines[i][ind:])
                steps.append((wd, "\n".join(body)))
            else:
                steps.append((wd, r.group(2)))
        i += 1
    return steps if any("cargo test" in b for _, b in steps) else []


def script_for(steps):
    out = ["set -euo pipefail"]
    for n, (wd, body) in enumerate(steps, 1):
        out.append("echo " + shlex.quote(f"--- step {n}: {wd}: {body.splitlines()[0][:70]}"))
        out.append(f"(cd /work/{wd}\n{body}\n)")
    return "\n".join(out)


def sh(*a, **kw):
    return subprocess.run(a, capture_output=True, text=True, **kw)


def verdict(word, t0, why=""):
    print(f"linux: {word}{' (' + why + ')' if why else ''} {time.time() - t0:.0f}s", flush=True)
    return 1 if word == "FAIL" else 0


def ensure_docker(profile):
    """Returns an env for docker, or a reason string."""
    if not shutil.which("docker"):
        return "docker not installed"
    if not shutil.which("colima"):
        return "colima not installed"
    env = {**os.environ}
    env.setdefault("DOCKER_CONTEXT", f"colima-{profile}")
    if sh("docker", "info", env=env).returncode == 0:
        return env
    print(f"linux: starting colima profile {profile}", flush=True)
    r = sh("colima", "start", "-p", profile, "--cpu", os.environ.get("PATCHBAY_CI_COLIMA_CPU", "6"),
           "--memory", os.environ.get("PATCHBAY_CI_COLIMA_MEMORY", "10"), "--runtime", "docker")
    if r.returncode or sh("docker", "info", env=env).returncode:
        return f"colima start -p {profile} failed: {(r.stderr or r.stdout).strip()[-200:]}"
    return env


def take_slot():
    """Hold one cargo-slot lock (the same files cargo-slot uses) so the pool still bounds builds."""
    root = Path(os.environ.get("CARGO_SLOT_ROOT") or Path.home() / ".cache" / "cargo-slots")
    n = int(os.environ.get("CARGO_SLOT_COUNT") or 3)
    fhs = []
    for i in range(1, n + 1):
        d = root / f"slot-{i}"
        d.mkdir(parents=True, exist_ok=True)
        fhs.append(open(d / ".cargo-slot.lock", "a+"))
    while True:
        for fh in fhs:
            try:
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return fh
            except OSError:
                pass
        time.sleep(2)


def main():
    t0 = time.time()
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--colima-profile", default="patchbay-ci")
    args = ap.parse_args()
    repo = Path(args.repo).resolve()
    try:
        steps = linux_steps((repo / WORKFLOW).read_text())
        pkgs = apt_packages((repo / DEPS).read_text())
    except OSError as e:
        return verdict("FAIL", t0, f"cannot read {e.filename}")
    if not steps or not pkgs:
        return verdict("FAIL", t0, f"parsed no cargo test steps or apt packages from {WORKFLOW}/{DEPS}")
    script = script_for(steps)
    if args.dry_run:
        print(f"packages: {' '.join(pkgs)}\n{script}")
        return verdict("PASS", t0, "dry run")
    env = ensure_docker(args.colima_profile)
    if isinstance(env, str):
        return verdict("NOT RUN", t0, env)
    # Tagged by Dockerfile hash (plus the package list it is built with): rebuilt only when either changes.
    tag = hashlib.sha256((" ".join(pkgs) + DOCKERFILE.read_text()).encode()).hexdigest()[:10]
    image = f"{IMAGE}:{tag}"
    if sh("docker", "image", "inspect", image, env=env).returncode:
        print(f"linux: building {image} (cold: apt, git PPA, rust)", flush=True)
        r = subprocess.run(["docker", "build", "-t", image, "-f", str(DOCKERFILE), "--build-arg",
                            f"APT_PACKAGES={' '.join(pkgs)}", str(DOCKERFILE.parent)], env=env)
        if r.returncode:
            return verdict("FAIL", t0, "image build failed")
    slot = take_slot()
    # The file list comes from the host: a worktree's /src/.git points at a host path the container lacks.
    files = sh("git", "-C", str(repo), "ls-files", "-co", "--exclude-standard", "-z").stdout
    # shortcut: no rsync --delete, a file deleted from the repo lingers in /work until the volume is dropped.
    # The repo is mounted read-only and rsynced into /work, so the checks cannot touch it; rsync keeps
    # mtimes, so cargo's fingerprints stay warm.
    inner = "mkdir -p /work && rsync -a --from0 --files-from=- /src/ /work/ && exec bash -c " + shlex.quote(script)
    r = subprocess.run(["docker", "run", "--rm", "-i", "-v", f"{repo}:/src:ro", "-v", f"{VOLUMES[0]}:/cache/cargo",
                        "-v", f"{VOLUMES[1]}:/cache/target", "-e", "CARGO_TARGET_DIR=/cache/target",
                        "-e", f"CARGO_BUILD_JOBS={os.environ.get('CARGO_BUILD_JOBS', '6')}", "-e", "CARGO_TERM_COLOR=never",
                        "-e", "GITHUB_ACTIONS=true", image, "bash", "-c", inner], env=env, input=files, text=True)
    slot.close()
    return verdict("PASS" if r.returncode == 0 else "FAIL", t0)


if __name__ == "__main__":
    sys.exit(main())
