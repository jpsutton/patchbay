#!/usr/bin/env bash
# Prepare Patchbay.app on macOS by removing the Gatekeeper quarantine
# attribute and applying a local ad-hoc signature so it opens without
# "app is damaged" warnings on macOS Sonoma / Sequoia.
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Error: This script is only intended for macOS." >&2
  exit 1
fi

APP_PATH=""
SHOULD_OPEN=false

for arg in "$@"; do
  case "$arg" in
    -o|--open)
      SHOULD_OPEN=true
      ;;
    -h|--help)
      echo "Usage: $(basename "$0") [options] [/path/to/Patchbay.app]"
      echo ""
      echo "Prepares Patchbay.app on macOS by removing the Gatekeeper quarantine"
      echo "attribute and applying a local ad-hoc code signature."
      echo ""
      echo "Options:"
      echo "  -o, --open    Open Patchbay after preparation"
      echo "  -h, --help    Show this help message"
      exit 0
      ;;
    *)
      if [ -z "$APP_PATH" ]; then
        APP_PATH="$arg"
      else
        echo "Error: Unexpected argument '$arg'" >&2
        exit 1
      fi
      ;;
  esac
done

if [ -z "$APP_PATH" ]; then
  if [ -d "/Applications/Patchbay.app" ]; then
    APP_PATH="/Applications/Patchbay.app"
  elif [ -d "$HOME/Applications/Patchbay.app" ]; then
    APP_PATH="$HOME/Applications/Patchbay.app"
  else
    echo "Error: Patchbay.app not found in /Applications or $HOME/Applications." >&2
    echo "Please drag Patchbay.app to /Applications first, or specify its path:" >&2
    echo "  $0 /path/to/Patchbay.app" >&2
    exit 1
  fi
fi

if [ ! -d "$APP_PATH" ]; then
  echo "Error: Directory not found: $APP_PATH" >&2
  exit 1
fi

echo "Clearing quarantine attributes on $APP_PATH..."
xattr -cr "$APP_PATH"

echo "Signing $APP_PATH with local ad-hoc signature..."
codesign --force --deep --sign - "$APP_PATH"

echo "Verifying code signature..."
if codesign --verify --deep --strict "$APP_PATH" 2>/dev/null; then
  echo "✓ Patchbay is ready to launch."
else
  echo "Warning: Code signature verification had warnings, but quarantine was cleared." >&2
fi

if [ "$SHOULD_OPEN" = true ]; then
  echo "Launching $APP_PATH..."
  open "$APP_PATH"
else
  echo "You can now open Patchbay from Applications, Spotlight, or by running:"
  echo "  open \"$APP_PATH\""
fi
