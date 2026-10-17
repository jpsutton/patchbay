#!/usr/bin/env bash
# Sourced by the CI scripts that run in an archlinux container (as root):
# the packages Patchbay and the grok sidecar build with.
set -euo pipefail
pacman -Syu --noconfirm --needed \
  base-devel git rust nodejs pnpm protobuf \
  webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg dbus xdotool >/dev/null
