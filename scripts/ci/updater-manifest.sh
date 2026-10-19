#!/usr/bin/env bash
# Usage: scripts/ci/updater-manifest.sh <dir> <version>
#
# Writes <dir>/latest.json, the feed the in-app updater reads (plugins.updater in
# src-tauri/tauri.conf.json), for the builds that can replace themselves: the macOS app
# tarball, the Windows installers and the AppImage. Each gets a minisign signature from
# TAURI_SIGNING_PRIVATE_KEY (and TAURI_SIGNING_PRIVATE_KEY_PASSWORD), embedded in the feed.
# The .sig files tauri writes are removed again: sign-release.sh puts its GPG signatures
# under the same names.
#
# Platforms whose files aren't in <dir> are left out, so a release without the best-effort
# macOS or Windows builds still updates the rest.
set -euo pipefail

dir=$1
version=$2
base="https://github.com/${GITHUB_REPOSITORY:-jpsutton/patchbay}/releases/download/v$version"

[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ] || { echo "TAURI_SIGNING_PRIVATE_KEY is not set" >&2; exit 1; }

# Updater platform key, then the file pattern for it.
targets=(
  darwin-aarch64-app  "Patchbay_${version}_aarch64.app.tar.gz"
  windows-x86_64-nsis "Patchbay_${version}_x64-setup.exe"
  windows-x86_64-msi  "Patchbay_${version}_x64_en-US.msi"
  linux-x86_64-appimage "Patchbay_${version}_amd64.AppImage"
)

platforms='{}'
for ((i = 0; i < ${#targets[@]}; i += 2)); do
  key=${targets[i]}
  file=${targets[i + 1]}
  if [ ! -f "$dir/$file" ]; then
    echo "::warning::$file not found; the updater feed leaves out $key"
    continue
  fi
  npx --yes @tauri-apps/cli@2 signer sign "$dir/$file" >/dev/null
  sig=$(cat "$dir/$file.sig")
  rm -f "$dir/$file.sig"
  platforms=$(jq --arg k "$key" --arg sig "$sig" --arg url "$base/$file" \
    '. + {($k): {signature: $sig, url: $url}}' <<<"$platforms")
done

[ "$platforms" != '{}' ] || { echo "no updater files in $dir" >&2; exit 1; }

# The updater tries <os>-<arch>-<bundle> first, then <os>-<arch> when the build doesn't know its
# bundle type. Point the plain keys at the default bundle for each system.
platforms=$(jq '
  (if ."darwin-aarch64-app" then . + {"darwin-aarch64": ."darwin-aarch64-app"} else . end)
  | (if ."windows-x86_64-nsis" then . + {"windows-x86_64": ."windows-x86_64-nsis"} else . end)
  | (if ."linux-x86_64-appimage" then . + {"linux-x86_64": ."linux-x86_64-appimage"} else . end)
' <<<"$platforms")

jq -n --arg v "$version" --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson p "$platforms" \
  '{version: $v, notes: "See the release notes on GitHub.", pub_date: $date, platforms: $p}' \
  > "$dir/latest.json"
cat "$dir/latest.json"
