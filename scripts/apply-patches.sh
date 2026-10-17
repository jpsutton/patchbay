#!/usr/bin/env bash
# Apply patches/*.patch to the vendored grok-build checkout. Idempotent: patches that are
# already applied are skipped. Both the sidecar and the in-process sampler build from this tree.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
vendor="$root/vendor/grok-build"

shopt -s nullglob
for patch in "$root"/patches/*.patch; do
  name="$(basename "$patch")"
  if git -C "$vendor" apply --reverse --check "$patch" 2>/dev/null; then
    echo "already applied: $name"
  elif git -C "$vendor" apply --check "$patch" 2>/dev/null; then
    git -C "$vendor" apply "$patch"
    echo "applied: $name"
  else
    echo "error: $name does not apply to vendor/grok-build" >&2
    exit 1
  fi
done
