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
import os
import re
import subprocess
import sys

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


BLOCK_START = re.compile(r"^\s*(Running|Doc-tests)\b")
RUNNING = re.compile(r"^running \d+ tests?$")
# cargo names each test exe on its header line. Newer cargo keeps it under target\debug\build\...,
# older under target\debug\deps, so the path is read from the output rather than guessed.
EXE_PATH = re.compile(r"\((?P<exe>[^()]+\.exe)\)\s*$")


def parse_leg(lines):
    """What one `cargo test` run printed, per test binary (cargo's `Running ...` / `Doc-tests ...`
    header starts each one): the failing and passing test names, the `test result:` totals, and
    whether every binary that started also finished.

    A test name is only taken from the lines before the binary's first `failures:`: after it comes
    the captured output of the failed tests, and a test that runs a child test process (for example
    the_real_home_hatch) prints that child's own `test ... FAILED` and `test result:` lines there.
    A binary's result is its LAST `test result:` line, which is its own."""
    blocks = [{"started": False, "result": None}]  # the implicit first block holds the preamble
    failed, passed, exes = [], [], []
    in_names = True
    for raw in lines:
        line = ANSI.sub("", raw).rstrip()
        if BLOCK_START.match(line):
            blocks.append({"started": False, "result": None})
            in_names = True
            m = EXE_PATH.search(line)
            if m:
                exes.append(m.group("exe"))
            continue
        block = blocks[-1]
        if line == "failures:":
            in_names = False
        if in_names:
            if RUNNING.match(line):
                block["started"] = True
            m = TEST_LINE.match(line)
            if m:
                if m.group(2) == "FAILED":
                    failed.append(m.group(1))
                elif m.group(2) == "ok":
                    passed.append(m.group(1))
        m = RESULT_LINE.match(line)
        if m:
            block["result"] = (int(m.group(2)), int(m.group(3)), int(m.group(4)))
    done = [blk["result"] for blk in blocks if blk["result"]]
    totals = {
        "passed": sum(r[0] for r in done),
        "failed": sum(r[1] for r in done),
        "ignored": sum(r[2] for r in done),
    }
    # Finished: something reported a result, and no binary that started is missing its result.
    finished = bool(done) and all(blk["result"] for blk in blocks if blk["started"])
    return {"failed": failed, "passed": passed, "totals": totals, "finished": finished, "exes": exes}


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


def manifest_problems(label, exes, folder):
    """Every test exe a src-tauri leg ran (the paths in cargo's own output, relative to `folder`)
    must have `<exe>.manifest` beside it: that file is what gives the exe Common-Controls v6."""
    lib_exes = [e for e in exes if os.path.basename(e).startswith("patchbay_lib-")]
    if not lib_exes:
        return [f"{label}: cargo's output names no patchbay_lib test exe, so its manifest cannot be checked"]
    out = []
    for exe in lib_exes:
        path = os.path.join(folder, exe)
        if not os.path.exists(path):
            out.append(f"{label}: {os.path.basename(exe)} (named by cargo) does not exist at {path}")
        elif not os.path.exists(path + ".manifest"):
            out.append(f"{label}: {os.path.basename(exe)} has no {os.path.basename(exe)}.manifest beside it")
    return out


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
    baseline = read_baseline(BASELINE)
    parsed_by_leg, codes = {}, {}
    for label, folder, args in LEGS:
        parsed_by_leg[label], codes[label] = run_leg(label, folder, args)
    manifests = []
    for label, folder, _ in LEGS:
        if label.startswith("src-tauri"):
            manifests += manifest_problems(label, parsed_by_leg[label]["exes"], os.path.join(ROOT, folder))
    new, fixed, problems = judge(parsed_by_leg, baseline, manifests)
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
