#!/usr/bin/env bash
# Local pre-push gate: THIRD_PARTY_NOTICES.md must match a fresh `pnpm notices` generation.
# Mirrors the "Third-party notices are up to date" step in .github/workflows/checks.yml.
# Called by package.json's "ci" script, which the global pre-push template
# (~/.git-templates/hooks/pre-push) runs as `pnpm run ci`. Takes ~1.5s.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1
file=THIRD_PARTY_NOTICES.md

# Snapshot so a failed check never leaves the working tree changed.
snapshot=$(mktemp)
trap 'rm -f "$snapshot"' EXIT
cp "$file" "$snapshot"

fail() {
  cp "$snapshot" "$file"
  echo "FAIL third-party notices are up to date" >&2
  echo "    THIRD_PARTY_NOTICES.md does not match a fresh generation." >&2
  echo "    Run \`pnpm notices\` and commit the updated THIRD_PARTY_NOTICES.md." >&2
  exit 1
}

pnpm notices >/dev/null || fail
# As in CI: after regenerating, the file must be what git would push.
git diff --exit-code -- "$file" >/dev/null || fail
# Regeneration can silently repair an edited or stale file, so also require that it
# produced no change at all (this is what the fail-loud corruption check exercises).
# Line-ending CRs are stripped on both sides first: with core.autocrlf=true
# (Windows default) the worktree copy can be CRLF while regeneration writes LF —
# a line-ending difference, not a content one (pb-20261106-13, defect #2).
# sed with a literal CR (built by printf), not `tr -d '\r'`: only line-ending
# CRs may be ignored, so a lone mid-line CR edit must still fail the check.
cr=$(printf '\r')
cmp -s <(sed "s/${cr}\$//" "$snapshot") <(sed "s/${cr}\$//" "$file") || fail

echo "ok   third-party notices are up to date"
