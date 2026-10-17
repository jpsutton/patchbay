#!/usr/bin/env bash
# CI: system packages for building Patchbay and the grok sidecar on Ubuntu
# (GitHub's ubuntu runners). Tauri 2 needs WebKitGTK 4.1. Grok Build also needs
# protoc 3.15+, which build.yml installs (Ubuntu 22.04's is too old).
set -euo pipefail
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
  build-essential curl file patchelf pkg-config \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev \
  libssl-dev libdbus-1-dev libxdo-dev >/dev/null
