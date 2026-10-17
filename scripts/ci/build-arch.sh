#!/usr/bin/env bash
# CI (build): Arch package via packaging/linux/arch/PKGBUILD, written to out/.
#   docker run --rm -v "$PWD:/src" -w /src archlinux:latest scripts/ci/build-arch.sh
set -euo pipefail
# shellcheck source=scripts/ci/deps-arch.sh
source "$(dirname "$0")/deps-arch.sh"

# makepkg refuses to run as root.
id builder >/dev/null 2>&1 || useradd -m builder
chown -R builder: "$PWD"
su builder -c 'git config --global --add safe.directory "*" && cd packaging/linux/arch && makepkg -f --nodeps --noconfirm'
mkdir -p out
mv packaging/linux/arch/*.pkg.tar.zst out/
ls -la out
