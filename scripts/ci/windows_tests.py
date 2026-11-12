#!/usr/bin/env python3
"""cargo test on Windows, judged against a recorded baseline (pb-20261111-63).

Runs the Rust test legs on the Windows runner, collects every failing test and compares it with
scripts/ci/windows-test-baseline.txt:

  * a failure that is NOT in the baseline fails the job (loud),
  * a baseline entry that now passes is reported, so the baseline can shrink,
  * a leg that did not finish (the exe did not start, a compile error, a crash) counts as a
    failure of its own, because "nothing ran" must never read as "nothing failed",
  * a leg that ran zero tests fails,
  * the guard test of the manifest fix (pb-20261111-58) must have run and passed in every
    src-tauri leg, and every src-tauri lib test exe built in this run must have its sibling
    `<exe>.manifest`: that fix is a side-by-side manifest file, so the tests must run IN PLACE from
    the build directory. Never copy or archive the test exes (nextest archives, artifact moves).

The baseline lists the failures of the job's FIRST run on windows-latest. The job is not a
required check while the baseline has entries; when it is empty, make it required.

Usage: python scripts/ci/windows_tests.py   (from anywhere; paths are relative to the repo)
"""
import glob
import os
import re
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BASELINE = os.path.join(ROOT, "scripts", "ci", "windows-test-baseline.txt")
LOGS = os.path.join(ROOT, "windows-test-logs")

# The guard test added by pb-20261111-58 (src-tauri/src/test_support.rs).
CANARY = "test_support::the_test_exe_gets_common_controls_v6"

# (label, folder, cargo test arguments). --no-fail-fast: every failure must be listed, not only
# the first test binary's.
LEGS = [
    ("src-tauri default", "src-tauri", ["--lib", "--locked"]),
    ("src-tauri ipc-test", "src-tauri", ["--lib", "--locked", "--features", "ipc-test"]),
    ("src-tauri web", "src-tauri", ["--lib", "--locked", "--no-default-features", "--features", "web"]),
    ("patchbay-mesh", "crates/patchbay-mesh", ["--locked"]),
    ("patchbay-llm", "crates/patchbay-llm", ["--locked"]),
    ("patchbay-agent", "crates/patchbay-agent", ["--locked"]),
]

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
TEST_LINE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)\b")
RESULT_LINE = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored")

DID_NOT_FINISH = "<leg did not finish>"
RAN_NO_TESTS = "<leg ran no tests>"


def parse_leg(lines):
    """What one `cargo test` run printed: failing and passing test names, the `test result:`
    totals, and whether any test binary finished (printed a result line)."""
    failed, passed = [], []
    totals = {"passed": 0, "failed": 0, "ignored": 0}
    results = 0
    for raw in lines:
        line = ANSI.sub("", raw).rstrip()
        m = TEST_LINE.match(line)
        if m:
            if m.group(2) == "FAILED":
                failed.append(m.group(1))
            elif m.group(2) == "ok":
                passed.append(m.group(1))
            continue
        m = RESULT_LINE.match(line)
        if m:
            results += 1
            totals["passed"] += int(m.group(2))
            totals["failed"] += int(m.group(3))
            totals["ignored"] += int(m.group(4))
    return {"failed": failed, "passed": passed, "totals": totals, "finished": results > 0}


def leg_failures(label, parsed):
    """The failure keys of one leg, `<leg>: <test>`, including the two pseudo failures."""
    keys = [f"{label}: {name}" for name in parsed["failed"]]
    if not parsed["finished"]:
        keys.append(f"{label}: {DID_NOT_FINISH}")
    elif parsed["totals"]["passed"] + parsed["totals"]["failed"] == 0:
        keys.append(f"{label}: {RAN_NO_TESTS}")
    return keys


def read_baseline(path):
    keys = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                keys.add(line)
    return keys


def manifest_problems(deps_dir, since):
    """Every `patchbay_lib-*.exe` built at or after `since` must have `<exe>.manifest` beside it."""
    exes = [p for p in glob.glob(os.path.join(deps_dir, "patchbay_lib-*.exe")) if os.path.getmtime(p) >= since]
    if not exes:
        return [f"no patchbay_lib test exe was built under {deps_dir} in this run"]
    return [
        f"{os.path.basename(exe)} has no {os.path.basename(exe)}.manifest beside it"
        for exe in exes
        if not os.path.exists(exe + ".manifest")
    ]


def judge(parsed_by_leg, baseline, manifest_issues):
    """(new failures, baseline entries that now pass, other problems). Fails when new or problems."""
    failures = set()
    problems = list(manifest_issues)
    for label, parsed in parsed_by_leg.items():
        failures.update(leg_failures(label, parsed))
        if label.startswith("src-tauri"):
            if CANARY in parsed["failed"]:
                problems.append(f"{label}: the Common-Controls guard test FAILED ({CANARY})")
            elif CANARY not in parsed["passed"] and parsed["finished"]:
                problems.append(f"{label}: the Common-Controls guard test did not run ({CANARY})")
    return sorted(failures - baseline), sorted(baseline - failures), problems


def run_leg(label, folder, args):
    os.makedirs(LOGS, exist_ok=True)
    log = os.path.join(LOGS, re.sub(r"[^A-Za-z0-9_.-]+", "-", label) + ".log")
    env = dict(os.environ, CARGO_TERM_COLOR="never")
    print(f"::group::{label}: cargo test {' '.join(args)}", flush=True)
    lines = []
    with open(log, "w", encoding="utf-8") as out:
        proc = subprocess.Popen(
            ["cargo", "test", "--no-fail-fast", *args],
            cwd=os.path.join(ROOT, folder),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        for line in proc.stdout:
            lines.append(line)
            out.write(line)
            sys.stdout.write(line)
        code = proc.wait()
    print("::endgroup::", flush=True)
    print(f"{label}: exit code {code} ({hex(code & 0xFFFFFFFF)})", flush=True)
    return parse_leg(lines), code


def summary(parsed_by_leg, codes, new, fixed, problems, baseline):
    out = ["## Windows cargo test", "", "| leg | passed | failed | ignored | exit |", "|---|---|---|---|---|"]
    for label, parsed in parsed_by_leg.items():
        t = parsed["totals"]
        out.append(f"| {label} | {t['passed']} | {t['failed']} | {t['ignored']} | {codes[label]} |")
    out += ["", f"Baseline: {len(baseline)} known failing; new: {len(new)}; fixed since baseline: {len(fixed)}.", ""]
    for title, items in (("New failures (outside the baseline)", new), ("Baseline entries that now pass", fixed), ("Problems", problems)):
        if items:
            out += [f"### {title}", ""] + [f"- `{i}`" for i in items] + [""]
    failing = sorted({k for label, p in parsed_by_leg.items() for k in leg_failures(label, p)})
    out += ["### All failures of this run (the baseline format)", "", "```"] + failing + ["```", ""]
    return "\n".join(out)


def main():
    # The runner's console is not UTF-8: a test name must never crash the report.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    since = time.time()
    baseline = read_baseline(BASELINE)
    parsed_by_leg, codes = {}, {}
    for label, folder, args in LEGS:
        parsed_by_leg[label], codes[label] = run_leg(label, folder, args)
    deps = os.path.join(ROOT, "src-tauri", "target", "debug", "deps")
    new, fixed, problems = judge(parsed_by_leg, baseline, manifest_problems(deps, since))
    text = summary(parsed_by_leg, codes, new, fixed, problems, baseline)
    print(text, flush=True)
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as f:
            f.write(text + "\n")
    for key in new:
        print(f"::error::NEW FAILURE outside the baseline: {key}")
    for problem in problems:
        print(f"::error::{problem}")
    for key in fixed:
        print(f"::notice::now passes, remove from the baseline: {key}")
    return 1 if new or problems else 0


if __name__ == "__main__":
    sys.exit(main())
