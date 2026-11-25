import sys
import unittest
from unittest import mock
from pathlib import Path

import linux_gate

ROOT = Path(__file__).resolve().parents[2]

SAMPLE = """\
jobs:
  other:
    steps:
      - run: cargo test --not-this-one
  linux:
    steps:
      - uses: actions/checkout@v4
      - name: Unit tests
        working-directory: src-tauri
        run: cargo test --lib --locked
      - name: Multi
        run: |
          set -e
          cargo test --test x --locked
          grep -q ok log
  windows:
    steps:
      - run: cargo test --windows-only
"""


class LinuxGate(unittest.TestCase):
    def test_sample(self):
        self.assertEqual(linux_gate.linux_steps(SAMPLE), [
            ("src-tauri", "cargo test --lib --locked"),
            (".", "set -e\ncargo test --test x --locked\ngrep -q ok log"),
        ])

    def test_empty_when_nothing_parsed(self):
        self.assertEqual(linux_gate.linux_steps(""), [])
        self.assertEqual(linux_gate.linux_steps("jobs:\n  linux:\n    steps:\n      - run: echo hi\n"), [])

    def test_empty_list_fails(self):
        d = Path(__import__("tempfile").mkdtemp())
        (d / ".github/workflows").mkdir(parents=True)
        (d / ".github/workflows/checks.yml").write_text("jobs:\n")
        (d / "scripts/ci").mkdir(parents=True)
        (d / "scripts/ci/deps-ubuntu.sh").write_text("packages=(\n  a\n)\n")
        with mock.patch.object(sys, 'argv', ['x', '--repo', str(d)]):
            self.assertEqual(linux_gate.main(), 1)

    def test_live_checks_yml(self):
        steps = linux_gate.linux_steps((ROOT / linux_gate.WORKFLOW).read_text())
        self.assertIn(("src-tauri", "cargo test --lib --locked --features ipc-test"), steps)
        self.assertTrue(any("fd_lifecycle_stress" in b for _, b in steps))
        self.assertIn("libwebkit2gtk-4.1-dev", linux_gate.apt_packages((ROOT / linux_gate.DEPS).read_text()))


if __name__ == "__main__":
    unittest.main()
