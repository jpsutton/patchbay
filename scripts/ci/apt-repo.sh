#!/usr/bin/env bash
# Add signed .debs to the apt repository on the gh-pages branch (/apt) and publish it. One suite
# ("stable"), one component ("main"). Keeps the newest three versions of each package, and pushes
# gh-pages as one fresh commit each time, so the branch never grows past a few packages. The
# F-Droid repository (scripts/ci/fdroid-repo.sh) lives on the same branch; each script keeps the
# other's files.
#
#   scripts/ci/apt-repo.sh dist/*_arm64.deb
#
# Needs apt-ftparchive (apt-utils) and gpg, and in the environment:
#   PATCHBAY_GPG_KEY        the release key, armored private key without a passphrase
#   GITHUB_REPOSITORY       owner/name, for the Pages URL
#   PATCHBAY_PAGES_REMOTE   optional: where gh-pages lives (default: origin), for trying this
#                           against a local repository
set -euo pipefail

src=$(cd "$(dirname "$0")/../.." && pwd)
: "${PATCHBAY_GPG_KEY:?}" "${GITHUB_REPOSITORY:?}"
owner=${GITHUB_REPOSITORY%%/*}
name=${GITHUB_REPOSITORY#*/}
url="https://${owner,,}.github.io/$name/apt"
remote=${PATCHBAY_PAGES_REMOTE:-$(git -C "$src" remote get-url origin)}
debs=()
for f in "$@"; do debs+=("$(realpath "$f")"); done
((${#debs[@]})) || { echo "usage: apt-repo.sh <deb>..." >&2; exit 1; }

work=$(mktemp -d)
export GNUPGHOME="$work/gnupg"
trap 'gpgconf --kill all; rm -rf "$work"' EXIT
mkdir -m 700 "$GNUPGHOME"
pages="$work/pages"
mkdir -p "$pages"
# Start from what is published, so the older versions stay listed.
if git -C "$src" fetch --depth 1 "$remote" gh-pages 2>/dev/null; then
    git -C "$src" archive FETCH_HEAD | tar -x -C "$pages"
fi
gpg --batch --quiet --import <<<"$PATCHBAY_GPG_KEY"
fpr=$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr/ {print $10; exit}')

apt="$pages/apt"
mkdir -p "$apt/pool/main"
cp "${debs[@]}" "$apt/pool/main/"
# <package>_<version>_<arch>.deb: drop all but the newest three versions of each package.
(
    cd "$apt/pool/main"
    for pkg in $(printf '%s\n' ./*.deb | sed 's|^\./||; s/_.*//' | sort -u); do
        printf '%s\n' "${pkg}"_*.deb | sort -V | head -n -3 | xargs -r rm -v --
    done
)

archs=$(printf '%s\n' "$apt"/pool/main/*.deb | sed -n 's/.*_\([^_]*\)\.deb$/\1/p' | sort -u | tr '\n' ' ')
suite="$apt/dists/stable"
rm -rf "$suite"
cd "$apt"
for arch in $archs; do
    dir="$suite/main/binary-$arch"
    mkdir -p "$dir"
    apt-ftparchive --arch "$arch" packages pool > "$dir/Packages"
    gzip -9kn "$dir/Packages"
    xz -9k "$dir/Packages"
done
apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=Patchbay \
    -o APT::FTPArchive::Release::Label=Patchbay \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Components=main \
    -o "APT::FTPArchive::Release::Architectures=${archs% }" \
    release "$suite" > "$work/Release"
mv "$work/Release" "$suite/Release"
gpg --batch --yes --detach-sign --armor -o "$suite/Release.gpg" "$suite/Release"
gpg --batch --yes --clearsign -o "$suite/InRelease" "$suite/Release"
gpg --batch --yes --export "$fpr" > "$apt/patchbay-archive-keyring.gpg"
gpg --batch --yes --armor --export "$fpr" > "$apt/patchbay-archive-keyring.asc"

# Prove it: apt's own tools accept the signatures.
gpg --batch --quiet --verify "$suite/Release.gpg" "$suite/Release"
gpg --batch --quiet --verify "$suite/InRelease"

cat > "$apt/index.html" <<EOF2
<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Patchbay apt repository</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}pre{overflow-x:auto}</style>
<h1>Patchbay apt repository</h1>
<p>Debian 12 or newer and Ubuntu 24.04 or newer, on 64-bit ARM.</p>
<pre>curl -fsSL $url/patchbay-archive-keyring.gpg | sudo tee /usr/share/keyrings/patchbay.gpg &gt;/dev/null
echo "deb [signed-by=/usr/share/keyrings/patchbay.gpg] $url stable main" | sudo tee /etc/apt/sources.list.d/patchbay.list
sudo apt update &amp;&amp; sudo apt install patchbay</pre>
<p>Key fingerprint: <code>$fpr</code></p>
EOF2

cd "$pages"
touch .nojekyll
git init -q -b gh-pages
# The checkout's credentials (actions/checkout keeps them in the repository's own config).
{ git -C "$src" config --get-regexp '^http\..*\.extraheader$' || true; } | while read -r key value; do
    git config "$key" "$value"
done
git add -A
git -c user.name="github-actions[bot]" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    commit -q -m "apt repository: $(basename "${debs[0]}")"
git push -q -f "$remote" gh-pages
echo "apt repository at $url (key $fpr)"
