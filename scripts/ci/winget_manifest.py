#!/usr/bin/env python3
"""Write the winget singleton manifest for a patchbayd Windows release zip (pb-20261110-14).

    scripts/ci/winget_manifest.py --version 0.23.1 --zip patchbayd-0.23.1-windows-x86_64.zip \
        --url https://github.com/jpsutton/patchbay/releases/download/v0.23.1/patchbayd-0.23.1-windows-x86_64.zip \
        > packaging/winget/jpsutton.patchbayd.yaml

The hash and the path of patchbayd.exe inside the zip are read from the zip itself, never typed:
the release zip wraps everything in a folder named after the release, so the path changes with
every version and a stale one makes winget fail at install time.
"""
import argparse
import hashlib
import sys
import zipfile

ID = "jpsutton.patchbayd"


def exe_path(names: list[str]) -> str:
    found = [n for n in names if n.rsplit("/", 1)[-1] == "patchbayd.exe"]
    if len(found) != 1:
        raise SystemExit(f"winget_manifest: expected exactly one patchbayd.exe in the zip, found {found}")
    return found[0].replace("/", "\\")


def manifest(version: str, url: str, sha256: str, exe: str) -> str:
    return f"""# yaml-language-server: $schema=https://aka.ms/winget-manifest.singleton.1.6.0.schema.json
PackageIdentifier: {ID}
PackageVersion: {version}
PackageLocale: en-US
Publisher: Patchbay
PackageName: patchbayd
License: Apache-2.0
ShortDescription: Patchbay without a window: the headless daemon that serves the Patchbay UI to a browser.
PackageUrl: https://github.com/jpsutton/patchbay
Installers:
  - Architecture: x64
    InstallerType: zip
    NestedInstallerType: portable
    NestedInstallerFiles:
      - RelativeFilePath: {exe}
        PortableCommandAlias: patchbayd
    InstallerUrl: {url}
    InstallerSha256: {sha256.upper()}
    UpgradeBehavior: uninstallPrevious
ManifestType: singleton
ManifestVersion: 1.6.0
"""


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--zip", required=True, help="the release zip, already downloaded")
    ap.add_argument("--url", required=True, help="where winget will download that same zip")
    a = ap.parse_args()
    with open(a.zip, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    with zipfile.ZipFile(a.zip) as z:
        exe = exe_path(z.namelist())
    if a.version not in exe:
        raise SystemExit(f"winget_manifest: version {a.version} is not in the exe path {exe}; wrong zip?")
    sys.stdout.write(manifest(a.version, a.url, sha, exe))


if __name__ == "__main__":
    main()
