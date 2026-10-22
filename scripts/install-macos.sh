#!/usr/bin/env bash
# Installs the latest Patchbay release on macOS (Apple silicon):
#
#   curl -fsSL https://raw.githubusercontent.com/jpsutton/patchbay/release/scripts/install-macos.sh | bash
#
# The builds are ad-hoc signed but not notarized by Apple, so a .dmg downloaded in a
# browser gets the quarantine attribute and Gatekeeper blocks the first launch. curl
# doesn't set that attribute, so the app this script installs opens without a prompt.
#
# The download is checked against the release's SHA256SUMS. Installs to /Applications,
# or ~/Applications when /Applications isn't writable. Set PATCHBAY_INSTALL_DIR to
# choose another folder.
set -euo pipefail

base=https://github.com/jpsutton/patchbay/releases/latest/download

die() { echo "install-macos: $*" >&2; exit 1; }

[[ $(uname -s) == Darwin ]] || die "this script is for macOS"
[[ $(uname -m) == arm64 ]] || die "Patchbay ships for Apple silicon (arm64) only"

if [[ -n ${PATCHBAY_INSTALL_DIR:-} ]]; then
  dest=$PATCHBAY_INSTALL_DIR
elif [[ -w /Applications ]]; then
  dest=/Applications
else
  dest=$HOME/Applications
fi
mkdir -p "$dest"

work=$(mktemp -d)
mnt=$work/mnt
cleanup() {
  if [[ -d $mnt ]]; then hdiutil detach -quiet "$mnt" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS"
dmg=$(grep -oE 'Patchbay_[0-9][^ ]*_aarch64\.dmg$' "$work/SHA256SUMS" | head -n 1) \
  || die "no macOS .dmg in the latest release"

echo "Downloading $dmg"
curl -fL --progress-bar -o "$work/$dmg" "$base/$dmg"
(cd "$work" && grep " $dmg\$" SHA256SUMS | shasum -a 256 -c -s) || die "checksum mismatch for $dmg"

mkdir "$mnt"
# The .dmg carries the Apache-2.0 license as a license agreement, which hdiutil pages
# through less and then asks to accept on stdin. Under curl | bash that stdin is the
# rest of this script, so answer it here and skip the pager.
PAGER="cat" hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mnt" "$work/$dmg" \
  <<<y >/dev/null
[[ -d $mnt/Patchbay.app ]] || die "Patchbay.app not found in $dmg"

if pgrep -xq Patchbay; then
  echo "Patchbay is running. Restart it to use the new version."
fi
rm -rf "$dest/Patchbay.app"
ditto "$mnt/Patchbay.app" "$dest/Patchbay.app"

echo "Installed $dest/Patchbay.app"
