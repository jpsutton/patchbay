"""python3 -m unittest scripts/ci/test_windows_tests.py"""
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import windows_tests as w  # noqa: E402

OK_LEG = [
    "running 3 tests",
    "test a::fine ... ok",
    f"test {w.CANARY} ... ok",
    "test b::slow ... ignored",
    "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s",
]


def leg(failed=(), canary=True, result=True):
    lines = ["running tests", "test a::fine ... ok"]
    if canary:
        lines.append(f"test {w.CANARY} ... ok")
    lines += [f"test {name} ... FAILED" for name in failed]
    if result:
        n = len(failed)
        lines.append(f"test result: {'FAILED' if n else 'ok'}. 2 passed; {n} failed; 0 ignored; 0 measured; 0 filtered out")
    return w.parse_leg(lines)


class Parse(unittest.TestCase):
    def test_names_and_totals(self):
        p = w.parse_leg(OK_LEG)
        self.assertEqual(p["passed"], ["a::fine", w.CANARY])
        self.assertEqual(p["failed"], [])
        self.assertEqual(p["totals"], {"passed": 2, "failed": 0, "ignored": 1})
        self.assertTrue(p["finished"])

    def test_failed_tests_including_doctest_names_with_spaces(self):
        p = w.parse_leg(["test x::y ... FAILED", "test src/lib.rs - f (line 3) ... FAILED", "test result: FAILED. 0 passed; 2 failed; 0 ignored"])
        self.assertEqual(p["failed"], ["x::y", "src/lib.rs - f (line 3)"])

    def test_colour_codes_do_not_hide_a_failure(self):
        p = w.parse_leg(["test x::y ... \x1b[31mFAILED\x1b[0m", "test result: FAILED. 0 passed; 1 failed; 0 ignored"])
        self.assertEqual(p["failed"], ["x::y"])

    def test_several_test_binaries_add_up(self):
        p = w.parse_leg([
            "     Running unittests src\\lib.rs (target\\debug\\deps\\a.exe)", "running 3 tests",
            "test result: ok. 3 passed; 0 failed; 0 ignored",
            "   Doc-tests a", "running 4 tests", "test result: ok. 4 passed; 0 failed; 1 ignored",
        ])
        self.assertEqual(p["totals"], {"passed": 7, "failed": 0, "ignored": 1})
        self.assertTrue(p["finished"])

    def test_a_child_test_run_inside_a_failure_is_not_counted(self):
        # The shape of the real Windows log: a test that runs a child test process prints the
        # child's own `running`, `test ... FAILED` and `test result:` lines in its captured output.
        p = w.parse_leg([
            "     Running unittests src\\lib.rs (target\\debug\\deps\\a.exe)",
            "running 4 tests",
            "test a::ok ... ok",
            "test parent::spawns_child ... FAILED",
            "test other::broken ... FAILED",
            "",
            "failures:",
            "",
            "---- parent::spawns_child stdout ----",
            "running 2 tests",
            "test child::one ... ok",
            "test child::two ... FAILED",
            "failures:",
            "    child::two",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 482 filtered out",
            "---- other::broken stdout ----",
            "failures:",
            "    other::broken",
            "    parent::spawns_child",
            "",
            "test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out",
        ])
        self.assertEqual(p["failed"], ["parent::spawns_child", "other::broken"])
        self.assertEqual(p["passed"], ["a::ok"])
        self.assertEqual(p["totals"], {"passed": 1, "failed": 2, "ignored": 0})

    def test_a_test_binary_that_dies_mid_run_is_not_finished_even_if_another_finished(self):
        p = w.parse_leg([
            "     Running unittests src\\lib.rs (a.exe)", "running 5 tests", "test a ... ok",
            "   Doc-tests a", "running 1 test", "test result: ok. 1 passed; 0 failed; 0 ignored",
        ])
        self.assertFalse(p["finished"])


class NothingRan(unittest.TestCase):
    def test_an_exe_that_never_started_is_a_failure_not_silence(self):
        # 0xc0000139: the process died before main; cargo prints no `test result:` line.
        p = w.parse_leg(["Running unittests src\\lib.rs", "error: test failed, to rerun pass `--lib`"])
        self.assertEqual(w.leg_failures("src-tauri ipc-test", p), [f"src-tauri ipc-test: {w.DID_NOT_FINISH}"])

    def test_zero_tests_is_a_failure(self):
        p = w.parse_leg(["test result: ok. 0 passed; 0 failed; 0 ignored"])
        self.assertEqual(w.leg_failures("patchbay-llm", p), [f"patchbay-llm: {w.RAN_NO_TESTS}"])


class Judge(unittest.TestCase):
    def verdict(self, legs, baseline=(), manifests=()):
        return w.judge(legs, set(baseline), list(manifests))

    def test_green_when_nothing_fails(self):
        new, fixed, problems = self.verdict({"src-tauri default": leg()})
        self.assertEqual((new, fixed, problems), ([], [], []))

    def test_a_new_failure_outside_the_baseline_is_reported(self):
        new, fixed, problems = self.verdict({"src-tauri default": leg(failed=["m::t"])})
        self.assertEqual(new, ["src-tauri default: m::t"])

    def test_a_known_failure_is_not_new(self):
        new, fixed, problems = self.verdict({"src-tauri default": leg(failed=["m::t"])}, baseline={"src-tauri default: m::t"})
        self.assertEqual((new, fixed, problems), ([], [], []))

    def test_a_baseline_entry_that_passes_is_reported_as_fixed(self):
        new, fixed, problems = self.verdict({"src-tauri default": leg()}, baseline={"src-tauri default: m::t"})
        self.assertEqual((new, fixed), ([], ["src-tauri default: m::t"]))

    def test_the_same_test_failing_in_another_leg_is_new(self):
        legs = {"src-tauri default": leg(failed=["m::t"]), "src-tauri web": leg(failed=["m::t"])}
        new, _, _ = self.verdict(legs, baseline={"src-tauri default: m::t"})
        self.assertEqual(new, ["src-tauri web: m::t"])

    def test_the_guard_test_must_have_run_in_every_src_tauri_leg(self):
        new, _, problems = self.verdict({"src-tauri default": leg(canary=False)})
        self.assertTrue(any("did not run" in p for p in problems), problems)

    def test_the_guard_test_failing_is_a_problem_even_if_baselined(self):
        legs = {"src-tauri web": leg(failed=[w.CANARY])}
        new, _, problems = self.verdict(legs, baseline={f"src-tauri web: {w.CANARY}"})
        self.assertTrue(any("guard test FAILED" in p for p in problems), problems)

    def test_the_guard_test_is_not_required_of_the_crates(self):
        new, _, problems = self.verdict({"patchbay-llm": leg(canary=False)})
        self.assertEqual(problems, [])

    def test_manifest_problems_are_carried_into_the_verdict(self):
        _, _, problems = self.verdict({"src-tauri default": leg()}, manifests=["x.exe has no x.exe.manifest beside it"])
        self.assertEqual(problems, ["x.exe has no x.exe.manifest beside it"])


class Manifests(unittest.TestCase):
    def test_the_exe_path_comes_from_cargos_running_line_in_either_layout(self):
        # The real runner log: newer cargo keeps the test exe under target\\debug\\build\\...\\out.
        p = w.parse_leg([
            "     Running unittests src\\lib.rs (target\\debug\\build\\patchbay/286fd76e35d800d2\\out\\patchbay_lib-286fd76e35d800d2.exe)",
            "   Doc-tests a",
            "     Running unittests src/lib.rs (target/debug/deps/patchbay_lib-abc.exe)",
        ])
        self.assertEqual(p["exes"], [
            "target\\debug\\build\\patchbay/286fd76e35d800d2\\out\\patchbay_lib-286fd76e35d800d2.exe",
            "target/debug/deps/patchbay_lib-abc.exe",
        ])

    def test_an_exe_without_its_manifest_is_a_problem(self):
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "t", "out"))
            exe = os.path.join("t", "out", "patchbay_lib-1.exe")
            open(os.path.join(d, exe), "w").close()
            self.assertEqual(
                w.manifest_problems("src-tauri default", [exe], d),
                ["src-tauri default: patchbay_lib-1.exe has no patchbay_lib-1.exe.manifest beside it"],
            )
            open(os.path.join(d, exe + ".manifest"), "w").close()
            self.assertEqual(w.manifest_problems("src-tauri default", [exe], d), [])

    def test_an_exe_cargo_names_but_that_is_not_there_is_a_problem(self):
        with tempfile.TemporaryDirectory() as d:
            problems = w.manifest_problems("src-tauri web", ["t/patchbay_lib-2.exe"], d)
            self.assertEqual(len(problems), 1)
            self.assertIn("does not exist", problems[0])

    def test_no_lib_test_exe_named_at_all_is_a_problem_not_a_pass(self):
        self.assertEqual(len(w.manifest_problems("src-tauri default", [], ".")), 1)
        self.assertEqual(len(w.manifest_problems("src-tauri default", ["target/debug/other-1.exe"], ".")), 1)


class Baseline(unittest.TestCase):
    def test_comments_and_blank_lines_are_ignored(self):
        with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8") as f:
            f.write("# a comment\n\nsrc-tauri default: m::t\n  patchbay-llm: x  \n")
        try:
            self.assertEqual(w.read_baseline(f.name), {"src-tauri default: m::t", "patchbay-llm: x"})
        finally:
            os.unlink(f.name)

    def test_the_committed_baseline_parses(self):
        self.assertIsInstance(w.read_baseline(w.BASELINE), set)


if __name__ == "__main__":
    unittest.main()
