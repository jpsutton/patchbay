#!/usr/bin/env bash
# CI (build): Debian 12 .debs (the app and patchbayd), written to out/. Built on the native arm64
# runner inside a debian:12 container, so the packages need only glibc 2.36 and run on Debian 12
# and 13 and on Ubuntu 24.04 (arm64).
#   docker run --rm -v "$PWD:/src" -v "$HOME/.cargo-debian:/cargo" -w /src debian:12 scripts/ci/build-debian.sh
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends \
  build-essential ca-certificates curl file git patchelf pkg-config xz-utils \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev \
  libssl-dev libdbus-1-dev libxdo-dev >/dev/null

# Node comes from nodejs.org (Debian 12 has 18); pnpm from the version package.json pins.
case "$(uname -m)" in
  aarch64) node_arch=arm64 ;;
  x86_64) node_arch=x64 ;;
  *) echo "build-debian.sh: unsupported $(uname -m)" >&2; exit 1 ;;
esac
base=https://nodejs.org/dist/latest-v24.x
tarball=$(curl -fsSL "$base/SHASUMS256.txt" | sed -n "s/^.* \(node-v.*-linux-$node_arch\.tar\.xz\)$/\1/p")
curl --retry 5 --retry-all-errors -fsSL "$base/$tarball" | tar -xJ -C /usr/local --strip-components=1
npm install -g "pnpm@$(sed -n 's/^  "packageManager": "pnpm@\(.*\)",$/\1/p' package.json)" >/dev/null

# The registry cache is the only part of cargo's home that survives between runs.
export CARGO_HOME=${CARGO_HOME:-/cargo}
export RUSTUP_HOME=/rustup
curl --retry 5 --retry-all-errors -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
export PATH="$CARGO_HOME/bin:$PATH"

pnpm install --frozen-lockfile
pnpm tauri build --bundles deb
# patchbayd's package depends on glibc 2.39 by default, for the Ubuntu build.
PATCHBAYD_DEB_LIBC=2.36 scripts/package-daemon.sh deb

mkdir -p out
cp src-tauri/target/release/bundle/deb/*.deb src-tauri/target/release/bundle/daemon/*.deb out/
ls -la out
