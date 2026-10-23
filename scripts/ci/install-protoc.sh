#!/usr/bin/env bash
# CI: install protoc <version> from its GitHub release asset and put it on PATH.
# arduino/setup-protoc resolves the version by paging through every protobuf
# release over the API, and one empty page fails the job, so fetch the pinned
# asset by URL instead.
#   install-protoc.sh 29.3 [dest]   (dest defaults to $RUNNER_TEMP/protoc)
set -euo pipefail
version=${1:?usage: install-protoc.sh <version> [dest]}
dest=${2:-${RUNNER_TEMP:?RUNNER_TEMP is unset; pass a dest}/protoc}
case "${RUNNER_OS:-$(uname -s)}" in
  Linux) asset="protoc-$version-linux-x86_64.zip" ;;
  macOS | Darwin) asset="protoc-$version-osx-universal_binary.zip" ;;
  Windows | MINGW* | MSYS* | CYGWIN*) asset="protoc-$version-win64.zip" ;;
  *) echo "install-protoc.sh: unsupported OS ${RUNNER_OS:-$(uname -s)}" >&2; exit 1 ;;
esac
url="https://github.com/protocolbuffers/protobuf/releases/download/v$version/$asset"
mkdir -p "$dest"
zip="$dest/$asset"
curl --retry 5 --retry-all-errors -fsSL -o "$zip" "$url"
if command -v unzip >/dev/null; then
  unzip -qo "$zip" -d "$dest"
else
  7z x -y -o"$dest" "$zip" >/dev/null
fi
rm -f "$zip"
"$dest/bin/protoc" --version
if [[ -n "${GITHUB_PATH:-}" ]]; then
  echo "$dest/bin" >>"$GITHUB_PATH"
fi
