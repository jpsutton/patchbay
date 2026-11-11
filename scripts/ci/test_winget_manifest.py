"""python3 -m unittest scripts/ci/test_winget_manifest.py"""
import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import winget_manifest as w  # noqa: E402


def make_zip(path, root="patchbayd-1.2.3-windows-x86_64", exes=1):
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{root}/LICENSE", "x")
        z.writestr(f"{root}/web/index.html", "<html>")
        for i in range(exes):
            z.writestr(f"{root}/{'' if i == 0 else f'b{i}/'}patchbayd.exe", "MZ")


class WingetManifest(unittest.TestCase):
    def test_exe_path_keeps_the_release_folder_with_backslashes(self):
        self.assertEqual(w.exe_path(["r/LICENSE", "r/patchbayd.exe", "r/web/index.html"]), "r\\patchbayd.exe")

    def test_no_or_two_exes_is_an_error(self):
        for names in (["r/LICENSE"], ["r/patchbayd.exe", "r/b/patchbayd.exe"]):
            with self.assertRaises(SystemExit):
                w.exe_path(names)

    def test_cli_reads_hash_and_path_from_the_zip(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, "p.zip")
            make_zip(zp)
            out = subprocess.run(
                [sys.executable, os.path.join(HERE, "winget_manifest.py"), "--version", "1.2.3", "--zip", zp, "--url", "https://x/p.zip"],
                capture_output=True, text=True, check=True,
            ).stdout
            sha = hashlib.sha256(open(zp, "rb").read()).hexdigest().upper()
        self.assertIn(f"InstallerSha256: {sha}", out)
        self.assertIn("RelativeFilePath: patchbayd-1.2.3-windows-x86_64\\patchbayd.exe", out)
        self.assertIn("PortableCommandAlias: patchbayd", out)
        self.assertIn("UpgradeBehavior: uninstallPrevious", out)
        self.assertIn("PackageVersion: 1.2.3", out)

    def test_a_zip_for_another_version_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            zp = os.path.join(d, "p.zip")
            make_zip(zp)
            r = subprocess.run(
                [sys.executable, os.path.join(HERE, "winget_manifest.py"), "--version", "9.9.9", "--zip", zp, "--url", "u"],
                capture_output=True, text=True,
            )
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
