#!/usr/bin/env bash
# CI: system packages for building Patchbay on Ubuntu
# (GitHub's ubuntu runners). Tauri 2 needs WebKitGTK 4.1.
# The GStreamer plugins are bundled into the AppImage (bundleMediaFramework), so audio plays there.
set -euo pipefail
# The Ubuntu mirrors sometimes stall instead of failing (a run once sat in apt for 45
# minutes, normally ~40s). Short network timeouts, and a bounded retry of the whole step.
apt=(sudo apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
# shellcheck disable=SC2329  # run via declare -f
install() {
  "${apt[@]}" update -qq
  "${apt[@]}" install -y -qq --no-install-recommends \
    build-essential curl file patchelf pkg-config \
    libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev \
    libssl-dev libdbus-1-dev libxdo-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly gstreamer1.0-libav
}
for attempt in 1 2 3; do
  # `timeout` cannot run a shell function, so hand it a shell that defines both.
  if timeout 6m bash -c "$(declare -p apt; declare -f install); install"; then exit 0; fi
  echo "deps-ubuntu: attempt $attempt failed or timed out" >&2
done
exit 1
