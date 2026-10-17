#!/usr/bin/env bash
# CI (checks): stage an empty placeholder at src-tauri/binaries/grok-<triple>.
# tauri-build refuses to compile without the externalBin it names, and the unit
# tests never run the agent, so checks skip the ~800-crate sidecar build.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
triple="$(rustc -vV | sed -n 's/^host: //p')"
mkdir -p "$root/src-tauri/binaries"
touch "$root/src-tauri/binaries/grok-$triple"
