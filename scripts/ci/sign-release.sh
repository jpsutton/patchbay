#!/usr/bin/env bash
# CI (release): sign release assets and build a signed pacman repo.
#   sign-release.sh <dir>
# Needs PATCHBAY_GPG_KEY (armored private key; the release workflow passes the
# repo secret). For every file in <dir>: a detached .sig. Also SHA256SUMS
# (+ .sig), and patchbay.db/.files so Arch users can add
#   [patchbay]
#   Server = https://github.com/jpsutton/patchbay/releases/latest/download
# Runs in an archlinux container (repo-add).
set -euo pipefail
dir=${1:?asset dir}
: "${PATCHBAY_GPG_KEY:?PATCHBAY_GPG_KEY is not set}"

pacman -Sy --noconfirm --needed gnupg pacman-contrib >/dev/null
export GNUPGHOME
GNUPGHOME=$(mktemp -d)
trap 'gpgconf --kill all; rm -rf "$GNUPGHOME"' EXIT
gpg --batch --quiet --import <<<"$PATCHBAY_GPG_KEY"
fpr=$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr/ {print $10; exit}')
echo "signing with $fpr"

cd "$dir"
rm -f ./*.sig patchbay.db* patchbay.files* SHA256SUMS*

shopt -s nullglob
pkgs=(./*.pkg.tar.zst)
if ((${#pkgs[@]})); then
  repo-add --sign --key "$fpr" patchbay.db.tar.zst "${pkgs[@]}"
  # Release assets can't be symlinks.
  for link in patchbay.db patchbay.db.sig patchbay.files patchbay.files.sig; do
    [[ -L $link ]] && cp --remove-destination "$(readlink -f "$link")" "$link"
  done
fi

for file in ./*; do
  [[ -f $file && $file != *.sig ]] || continue
  [[ -e $file.sig ]] || gpg --batch --yes --detach-sign --no-armor "$file"
done
sha256sum -- * | grep -v ' SHA256SUMS' > SHA256SUMS
gpg --batch --yes --detach-sign --no-armor SHA256SUMS
gpg --batch --armor --export "$fpr" > patchbay-signing-key.asc

# Prove it: every signature verifies against the imported key.
for sig in ./*.sig; do
  gpg --batch --quiet --verify "$sig" "${sig%.sig}"
done
ls -la
