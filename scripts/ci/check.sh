#!/usr/bin/env bash
# Static checks that need no build: shell scripts, the desktop entry, the
# PKGBUILD, and that every place holding the app version agrees. CI runs this
# on every pull request.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
failures=0
check() {
  local label=$1 out
  shift
  if ! out=$("$@" 2>&1); then
    echo "FAIL $label"
    printf '    %s\n' "${out//$'\n'/$'\n'    }"
    failures=$((failures + 1))
  else
    echo "ok   $label"
  fi
}
for f in $(git ls-files '*.sh' 'packaging/linux/arch/PKGBUILD'); do
  check "bash -n $f" bash -n "$f"
done
for f in $(git ls-files '*.sh'); do
  check "shellcheck $f" shellcheck -x "$f"
done
for f in $(git ls-files '*.desktop'); do
  check "desktop-file-validate $f" desktop-file-validate "$f"
done
check "PKGBUILD" bash -c 'cd packaging/linux/arch && makepkg --printsrcinfo >/dev/null'
# shellcheck disable=SC2016 # expanded by the inner shell
check "patches apply to vendor/grok-build" bash -c '
  for p in patches/*.patch; do
    git -C vendor/grok-build apply --check "../../$p" 2>/dev/null \
      || git -C vendor/grok-build apply --reverse --check "../../$p" \
      || { echo "$p does not apply"; exit 1; }
  done'
check "versions agree" scripts/ci/version.sh
exit $((failures > 0))
