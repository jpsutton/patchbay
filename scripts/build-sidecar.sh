#!/usr/bin/env bash
# Build the Grok Build agent binary from the vendored submodule and stage it
# where Tauri's externalBin expects it: src-tauri/binaries/grok-<target-triple>.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${PROFILE:-debug}"
triple="$(rustc -vV | sed -n 's/^host: //p')"
target_dir="$root/target/grok"

cargo_args=(build -p xai-grok-pager-bin --target-dir "$target_dir")
[[ "$profile" == "release" ]] && cargo_args+=(--release)

# protoc on PATH is enough; DotSlash is not required.
export PROTOC="${PROTOC:-$(command -v protoc)}"

"$root/scripts/apply-patches.sh"
(cd "$root/vendor/grok-build" && cargo "${cargo_args[@]}")

ext=""
[[ "$triple" == *windows* ]] && ext=".exe"
mkdir -p "$root/src-tauri/binaries"
cp "$target_dir/$profile/xai-grok-pager$ext" "$root/src-tauri/binaries/grok-$triple$ext"
echo "staged src-tauri/binaries/grok-$triple$ext"
