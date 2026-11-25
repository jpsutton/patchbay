#!/usr/bin/env sh
# Thin wrapper for scripts/ci/linux_gate.py (ADR-002).
set -eu
command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 1; }
exec python3 "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/linux_gate.py" "$@"
