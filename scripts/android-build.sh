#!/usr/bin/env bash
# Build Patchbay Remote's Android APK in the build container (remote/docker/Dockerfile), on a
# machine without the Android SDK. CI builds on GitHub's runner image instead.
#
#   scripts/android-build.sh           release APK for arm64, unsigned
#   scripts/android-build.sh --debug   debug APK for arm64, signed with the debug key (installable)
#
# ANDROID_TARGET=x86_64 builds for the emulator instead. The APK lands under
# remote/src-tauri/gen/android/app/build/outputs/apk/.
set -euo pipefail

cd "$(dirname "$0")/.."
here=$(pwd -P)
image=patchbay-android:local
cache=${XDG_CACHE_HOME:-$HOME/.cache}/patchbay-android

if ! docker image inspect "$image" >/dev/null 2>&1; then
    docker build -t "$image" remote/docker
fi

# The frontend builds on the host, with the checkout's own node_modules.
pnpm build:remote

# Mount the repository's top directory at its own path, so a worktree's links into it resolve.
top=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
mkdir -p "$cache/gradle" "$cache/android" "$HOME/.cargo/registry"
# ~/.android keeps the debug signing key, so a debug build installs over the previous one.
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp/home \
    -v "$top:$top" \
    -v "$cache/gradle:/tmp/home/.gradle" \
    -v "$cache/android:/tmp/home/.android" \
    -v "$HOME/.cargo/registry:/opt/cargo/registry" \
    -w "$here/remote" \
    "$image" \
    ../node_modules/.bin/tauri android build --apk --target "${ANDROID_TARGET:-aarch64}" --ci \
    --config '{"build":{"beforeBuildCommand":""}}' "$@"
