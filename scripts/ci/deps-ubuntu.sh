#!/usr/bin/env bash
# CI: system packages for building Patchbay on Ubuntu
# (GitHub's ubuntu runners). Tauri 2 needs WebKitGTK 4.1.
# The GStreamer plugins are bundled into the AppImage (bundleMediaFramework), so audio plays there.
set -euo pipefail
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
  build-essential curl file patchelf pkg-config \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev \
  libssl-dev libdbus-1-dev libxdo-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly gstreamer1.0-libav >/dev/null
