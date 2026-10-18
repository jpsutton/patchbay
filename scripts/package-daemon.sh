#!/usr/bin/env bash
# Build patchbayd (headless Patchbay with its browser UI) and pack it as a tarball:
#   patchbayd-<version>-linux-<arch>/
#     bin/patchbayd, bin/grok            the daemon and the Grok Build sidecar it runs
#     share/patchbay/web/                the built frontend it serves
#     share/patchbay/patchbayd.service   systemd user unit
# Output: src-tauri/target/release/bundle/daemon/patchbayd-<version>-linux-<arch>.tar.gz
# Expects the frontend (pnpm build) and the release sidecar (PROFILE=release
# scripts/build-sidecar.sh) to be built already, as the Linux CI job does before bundling.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml | head -n 1)"
triple="$(rustc -vV | sed -n 's/^host: //p')"
arch="${triple%%-*}"
name="patchbayd-$version-linux-$arch"

[[ -f dist/index.html ]] || { echo "dist/ is missing; run pnpm build first" >&2; exit 1; }
sidecar="src-tauri/binaries/grok-$triple"
[[ -x "$sidecar" ]] || { echo "$sidecar is missing; run PROFILE=release scripts/build-sidecar.sh" >&2; exit 1; }

(cd src-tauri && cargo build --release --locked --bin patchbayd --no-default-features --features web)

out="src-tauri/target/release/bundle/daemon"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
pkg="$stage/$name"
mkdir -p "$pkg/bin" "$pkg/share/patchbay"
install -m 755 src-tauri/target/release/patchbayd "$pkg/bin/patchbayd"
install -m 755 "$sidecar" "$pkg/bin/grok"
cp -r dist "$pkg/share/patchbay/web"
install -m 644 packaging/linux/daemon/patchbayd.service "$pkg/share/patchbay/patchbayd.service"
install -m 644 LICENSE NOTICE THIRD_PARTY_NOTICES.md "$pkg/"
install -m 644 docs/headless.md "$pkg/README.md"

mkdir -p "$out"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "packed $out/$name.tar.gz"
