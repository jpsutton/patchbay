#!/usr/bin/env bash
# Print the app version, after checking that package.json, src-tauri/Cargo.toml,
# src-tauri/tauri.conf.json and the PKGBUILD all hold the same one.
set -euo pipefail
cd "$(dirname "$0")/../.."
pkg=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' package.json)
cargo=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"$/\1/p}' src-tauri/Cargo.toml)
tauri=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' src-tauri/tauri.conf.json)
arch=$(sed -n 's/^pkgver=//p' packaging/linux/arch/PKGBUILD)
if [ -z "$pkg" ] || [ "$pkg" != "$cargo" ] || [ "$pkg" != "$tauri" ] || [ "$pkg" != "$arch" ]; then
  echo "version mismatch: package.json=$pkg Cargo.toml=$cargo tauri.conf.json=$tauri PKGBUILD=$arch" >&2
  exit 1
fi
echo "$pkg"
