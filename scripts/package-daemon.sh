#!/usr/bin/env bash
# Build patchbayd (headless Patchbay with its browser UI) and pack it.
#   package-daemon.sh [tar] [deb] [rpm] [zip]
# Default formats: tar, deb and rpm on Linux; tar on macOS; zip on Windows.
# Output: src-tauri/target/release/bundle/daemon/
#
# The daemon finds the web UI next to its own binary, so each format keeps the two together:
#   tar/zip   patchbayd-<version>-<os>-<arch>/  bin/patchbayd, share/patchbay/web/
#             (zip: patchbayd.exe and web/ side by side)
#   deb/rpm   /usr/lib/patchbayd/{patchbayd,web/}, /usr/bin/patchbayd, and systemd units
#             in /usr/lib/systemd/{user/patchbayd.service,system/patchbayd@.service}
# Expects the frontend (pnpm build) to be built already, as the CI jobs do before bundling.
# deb needs dpkg-deb, rpm needs rpmbuild.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml | head -n 1)"
triple="$(rustc -vV | sed -n 's/^host: //p')"
arch="${triple%%-*}"
ext=""
case "$triple" in
  *linux*) os=linux; formats=(tar deb rpm) ;;
  *darwin*) os=macos; formats=(tar) ;;
  *windows*) os=windows; formats=(zip); ext=".exe" ;;
  *) echo "unsupported host $triple" >&2; exit 1 ;;
esac
(($#)) && formats=("$@")
name="patchbayd-$version-$os-$arch"
summary="Patchbay, headless: chats, Code sessions and the mesh, with a browser UI"
description="patchbayd runs Patchbay without a window, for a server or any computer without a display. Use it from a browser, which gets the full Patchbay UI, or from the phone app and other computers in your mesh."

[[ -f dist/index.html ]] || { echo "dist/ is missing; run pnpm build first" >&2; exit 1; }

(cd src-tauri && cargo build --release --locked --bin patchbayd --no-default-features --features web)
daemon="src-tauri/target/release/patchbayd$ext"

out="src-tauri/target/release/bundle/daemon"
mkdir -p "$out"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

notices() {
  install -m 644 LICENSE NOTICE THIRD_PARTY_NOTICES.md "$1/"
}

# The layout deb and rpm install.
system_root() {
  local r="$stage/system"
  [[ -d "$r" ]] && { echo "$r"; return; }
  install -d "$r/usr/lib/patchbayd" "$r/usr/bin" "$r/usr/share/doc/patchbayd" "$r/usr/share/licenses/patchbayd"
  install -m 755 "$daemon" "$r/usr/lib/patchbayd/patchbayd"
  cp -r dist "$r/usr/lib/patchbayd/web"
  ln -s ../lib/patchbayd/patchbayd "$r/usr/bin/patchbayd"
  install -Dm 644 packaging/linux/daemon/patchbayd.service "$r/usr/lib/systemd/user/patchbayd.service"
  install -Dm 644 packaging/linux/daemon/patchbayd@.service "$r/usr/lib/systemd/system/patchbayd@.service"
  install -m 644 docs/headless.md "$r/usr/share/doc/patchbayd/README.md"
  # Not under doc/: installs without docs (e.g. Fedora containers) must keep the notices.
  notices "$r/usr/share/licenses/patchbayd"
  echo "$r"
}

pack_tar() {
  local pkg="$stage/tar/$name"
  install -d "$pkg/bin" "$pkg/share/patchbay"
  install -m 755 "$daemon" "$pkg/bin/patchbayd"
  cp -r dist "$pkg/share/patchbay/web"
  if [[ $os == linux ]]; then
    # The user unit, pointed at the folder docs/headless.md unpacks into.
    sed 's|^ExecStart=.*|ExecStart=%h/.local/opt/patchbayd/bin/patchbayd|' packaging/linux/daemon/patchbayd.service \
      > "$pkg/share/patchbay/patchbayd.service"
  fi
  install -m 644 docs/headless.md "$pkg/README.md"
  notices "$pkg"
  tar -C "$stage/tar" -czf "$out/$name.tar.gz" "$name"
  echo "packed $out/$name.tar.gz"
}

pack_zip() {
  local pkg="$stage/zip/$name"
  install -d "$pkg"
  install -m 755 "$daemon" "$pkg/patchbayd$ext"
  cp -r dist "$pkg/web"
  install -m 644 docs/headless.md "$pkg/README.md"
  notices "$pkg"
  rm -f "$out/$name.zip"
  if command -v zip >/dev/null; then
    (cd "$stage/zip" && zip -qr "$root/$out/$name.zip" "$name")
  else
    (cd "$stage/zip" && 7z a -tzip -bso0 "$root/$out/$name.zip" "$name")
  fi
  echo "packed $out/$name.zip"
}

pack_deb() {
  local r debarch pkg="$stage/deb"
  r="$(system_root)"
  case "$arch" in x86_64) debarch=amd64 ;; aarch64) debarch=arm64 ;; *) debarch="$arch" ;; esac
  cp -a "$r" "$pkg"
  install -d "$pkg/DEBIAN"
  cat > "$pkg/DEBIAN/control" <<CONTROL
Package: patchbayd
Version: $version
Architecture: $debarch
Maintainer: Patchbay <https://github.com/jpsutton/patchbay>
Depends: libc6 (>= 2.39), libdbus-1-3
Section: utils
Priority: optional
Homepage: https://github.com/jpsutton/patchbay
Description: $summary
 $description
CONTROL
  dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out/patchbayd_${version}_$debarch.deb" >/dev/null
  echo "packed $out/patchbayd_${version}_$debarch.deb"
}

pack_rpm() {
  local r top="$stage/rpm"
  r="$(system_root)"
  install -d "$top/SPECS" "$top/RPMS"
  cat > "$top/SPECS/patchbayd.spec" <<SPEC
%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none
Name: patchbayd
Version: $version
Release: 1
Summary: $summary
License: Apache-2.0
URL: https://github.com/jpsutton/patchbay
AutoReqProv: no
Requires: libdbus-1.so.3()(64bit)

%description
$description

%install
cp -a "$r/." "%{buildroot}/"

%files
/usr/bin/patchbayd
/usr/lib/patchbayd
/usr/lib/systemd/user/patchbayd.service
/usr/lib/systemd/system/patchbayd@.service
/usr/share/doc/patchbayd
/usr/share/licenses/patchbayd
SPEC
  rpmbuild -bb --quiet --target "$arch" --define "_topdir $top" "$top/SPECS/patchbayd.spec"
  find "$top/RPMS" -name '*.rpm' -exec mv -t "$out" {} +
  echo "packed $out/patchbayd-$version-1.$arch.rpm"
}

for f in "${formats[@]}"; do
  case "$f" in
    tar | zip | deb | rpm) "pack_$f" ;;
    *) echo "unknown format $f" >&2; exit 1 ;;
  esac
done
