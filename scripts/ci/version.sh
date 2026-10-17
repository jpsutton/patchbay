#!/usr/bin/env bash
# Print the app version, after checking that package.json, src-tauri/Cargo.toml,
# src-tauri/tauri.conf.json, the PKGBUILD, the phone app (remote/src-tauri) and the
# mesh protocol crate all hold the same one.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo_version() { sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"$/\1/p}' "$1"; }
conf_version() { sed -n 's/^  "version": "\(.*\)",$/\1/p' "$1"; }
pkg=$(conf_version package.json)
declare -A found=(
  [src-tauri/Cargo.toml]=$(cargo_version src-tauri/Cargo.toml)
  [src-tauri/tauri.conf.json]=$(conf_version src-tauri/tauri.conf.json)
  [PKGBUILD]=$(sed -n 's/^pkgver=//p' packaging/linux/arch/PKGBUILD)
  [remote/src-tauri/Cargo.toml]=$(cargo_version remote/src-tauri/Cargo.toml)
  [remote/src-tauri/tauri.conf.json]=$(conf_version remote/src-tauri/tauri.conf.json)
  [crates/patchbay-mesh/Cargo.toml]=$(cargo_version crates/patchbay-mesh/Cargo.toml)
)
bad=0
for f in "${!found[@]}"; do
  [ -n "$pkg" ] && [ "${found[$f]}" = "$pkg" ] || bad=1
done
if [ "$bad" = 1 ]; then
  echo "version mismatch: package.json=$pkg" >&2
  for f in "${!found[@]}"; do echo "  $f=${found[$f]}" >&2; done
  exit 1
fi
echo "$pkg"
