#!/usr/bin/env bash
# Patchbay Remote, end to end, in an Android emulator, against a desktop-side node built from this
# same checkout, so the phone and the computer it joins are always the same version.
#
#   scripts/android-e2e.sh doctor            what is installed and what is missing; changes nothing
#   scripts/android-e2e.sh setup [--only S]  install what is missing: jdk, cmdline-tools, sdk, avd, rust
#   scripts/android-e2e.sh up [options]      build, start everything, join the phone, check it shows the data
#   scripts/android-e2e.sh test [options]    up --fresh --headless, run the scripted phone scenarios, stop everything
#   scripts/android-e2e.sh status            what runs, and whether phone and node are the same source
#   scripts/android-e2e.sh down [--emulator] stop the node and the mock model (and the emulator)
#
# `test` options: --only a,b (some scenarios), --no-up (the stack is already up from `up --fresh`), --keep (leave it
# running), --no-build, --list. Scenarios the checkout has no code for are skipped; exit 1 only when one fails.
#
# `up` options: --fresh (new mesh, clean app data), --no-build, --headless (no emulator window),
# --cold (boot the emulator without its snapshot).
#
# One piece at a time: build [apk|node|all], emulator [start|stop] [--headless] [--cold], node [start|stop],
# install, join, verify, shot, logs.
#
# What `up` starts:
#   - scripts/mock-llm.py as the model, so no provider key is involved;
#   - patchbayd from this checkout (src-tauri, feature `web`) with its own data folder, a new mesh
#     that listens on 127.0.0.1 only, one chat and one Code session. The phone reaches it as 10.0.2.2.
#     Only a phone named "e2e-emulator" is approved, and only on that loopback listener;
#   - the debug APK built from this checkout, in its own AVD (API 36, Google APIs, Pixel 6). Installing and
#     --fresh only act on the AVD named above, never on an emulator you made yourself.
# The same node serves the desktop UI at http://127.0.0.1:4431/ (`status` prints the address).
#
# Versions come from remote/docker/Dockerfile (command-line tools, SDK platforms, build tools, NDK), so
# this and the Docker build cannot drift. State lives in $PATCHBAY_E2E_DIR (default ~/.cache/patchbay-android-e2e).
# Environment: PATCHBAY_E2E_AVD (patchbay-e2e), PATCHBAY_E2E_WEB_PORT (4431), PATCHBAY_E2E_MESH_PORT (41895),
# PATCHBAY_E2E_MOCK_PORT (8766), PATCHBAY_E2E_PYTHON (an interpreter to use instead of pipenv).
set -euo pipefail
# Run from a git hook (the local CI gate runs this at pre-push), git exports GIT_DIR and friends. Left
# set, the git commands below that make the sample project would act on the hook's repository instead.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_COMMON_DIR GIT_PREFIX GIT_ALTERNATE_OBJECT_DIRECTORIES

cd "$(dirname "$0")/.."
top=$(pwd -P)
helpers=$top/scripts/android-e2e

E2E_DIR=${PATCHBAY_E2E_DIR:-$HOME/.cache/patchbay-android-e2e}
AVD=${PATCHBAY_E2E_AVD:-patchbay-e2e}
WEB_PORT=${PATCHBAY_E2E_WEB_PORT:-4431}
MESH_PORT=${PATCHBAY_E2E_MESH_PORT:-41895}
MOCK_PORT=${PATCHBAY_E2E_MOCK_PORT:-8766}
APP_ID=dev.patchbay.remote
PHONE_NAME=e2e-emulator
DESKTOP_NAME=e2e-desktop
EMU_API=36
EMU_FLAVOR=google_apis
EMU_DEVICE=pixel_6
avd_home=${ANDROID_AVD_HOME:-$HOME/.android/avd}

say() { printf '==> %s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# ---- the host ----

case "$(uname -s)" in
    Darwin) os=mac ;;
    Linux) os=linux ;;
    *) die "this runs on macOS and Linux" ;;
esac
case "$(uname -m)" in
    arm64 | aarch64) abi=arm64-v8a; tauri_target=aarch64; rust_target=aarch64-linux-android; jdk_arch=aarch64 ;;
    x86_64) abi=x86_64; tauri_target=x86_64; rust_target=x86_64-linux-android; jdk_arch=x64 ;;
    *) die "unsupported CPU $(uname -m)" ;;
esac
image_pkg="system-images;android-$EMU_API;$EMU_FLAVOR;$abi"

if [ -n "${ANDROID_HOME:-}" ]; then sdk=$ANDROID_HOME
elif [ -n "${ANDROID_SDK_ROOT:-}" ]; then sdk=$ANDROID_SDK_ROOT
elif [ "$os" = mac ]; then sdk=$HOME/Library/Android/sdk
else sdk=$HOME/Android/Sdk
fi

# ---- pinned versions, read from the Docker build so there is one place to change them ----

dockerfile=remote/docker/Dockerfile
ndk_version=$(sed -n 's/^[[:space:]]*NDK_VERSION=\([0-9.][0-9.]*\).*/\1/p' "$dockerfile" | head -n1)
tools_build=$(sed -n 's/.*commandlinetools-linux-\([0-9][0-9]*\)_latest\.zip.*/\1/p' "$dockerfile" | head -n1)
[ -n "$ndk_version" ] && [ -n "$tools_build" ] || die "cannot read the pinned versions from $dockerfile"

# The packages Docker installs, one per line, plus the emulator and its image.
sdk_packages() {
    local line pkg
    line=$(grep 'sdkmanager "' "$dockerfile" | head -n1)
    for pkg in $(printf '%s' "$line" | grep -o '"[^"]*"' | tr -d '"'); do
        printf '%s\n' "${pkg//\$NDK_VERSION/$ndk_version}"
    done
    printf '%s\n%s\n' emulator "$image_pkg"
}
pkg_dir() { printf '%s/%s' "$sdk" "${1//;//}"; }

# ---- tools ----

java_major() { "$1/bin/java" -version 2>&1 | sed -n 's/.*version "\([0-9][0-9]*\).*/\1/p' | head -n1; }

# A JDK 21, wherever a package manager, Temurin's tarball or `setup` put it.
find_jdk() {
    local c
    for c in "${JAVA_HOME:-}" "$(/usr/libexec/java_home -v 21 2>/dev/null || true)" \
        "$HOME"/Library/Java/jdk-21*/Contents/Home "$HOME"/.local/share/java/jdk-21* /usr/lib/jvm/*21*; do
        if [ -n "$c" ] && [ -x "$c/bin/java" ] && [ "$(java_major "$c")" = 21 ]; then
            printf '%s\n' "$c"
            return 0
        fi
    done
    return 1
}

# The directory of the rustc rustup manages. A package manager's rustc may come first on PATH, and
# it has no Android targets: the build then fails with "can't find crate for core".
rust_bin() {
    have rustup || return 1
    local rc
    rc=$(rustup which rustc 2>/dev/null) || return 1
    dirname "$rc"
}

use_env() {
    local rb
    JAVA_HOME=$(find_jdk) || die "no JDK 21; run: scripts/android-e2e.sh setup --only jdk"
    export JAVA_HOME ANDROID_HOME=$sdk ANDROID_SDK_ROOT=$sdk NDK_HOME=$sdk/ndk/$ndk_version
    rb=$(rust_bin || true)
    PATH="${rb:+$rb:}$JAVA_HOME/bin:$sdk/cmdline-tools/latest/bin:$sdk/platform-tools:$sdk/emulator:$PATH"
    export PATH
}

ensure_state() {
    mkdir -p "$E2E_DIR/logs" "$E2E_DIR/shots" "$E2E_DIR/stamp"
    chmod 700 "$E2E_DIR"
}

# ---- doctor ----

fail=0
ok() { printf '  ok       %s\n' "$*"; }
miss() { printf '  MISSING  %s\n' "$*"; fail=1; }
note() { printf '  note     %s\n' "$*"; }

py_ready() { [ -n "${PATCHBAY_E2E_PYTHON:-}" ] || have pipenv; }

checks() {
    local targets p jdk node_major
    fail=0
    printf 'Host %s/%s, emulator image %s, AVD %s, SDK %s\n' "$os" "$(uname -m)" "$image_pkg" "$AVD" "$sdk"

    node_major=$(have node && node -p 'process.versions.node.split(".")[0]' || echo 0)
    if [ "$node_major" -ge 22 ]; then ok "node $(node -v)"; else miss "Node 22 or newer (the drivers use its WebSocket)"; fi
    for p in git pnpm; do
        if have "$p"; then ok "$p"; else miss "$p"; fi
    done
    if [ -x node_modules/.bin/tauri ]; then ok "node_modules (the Tauri CLI)"; else miss "node_modules: run pnpm install"; fi
    if have rustup && have cargo; then
        targets=$(rustup target list --installed 2>/dev/null || true)
        if printf '%s\n' "$targets" | grep -qx "$rust_target"; then ok "Rust target $rust_target"; else miss "Rust target $rust_target: run setup --only rust"; fi
    else
        miss "rustup and cargo (https://rustup.rs)"
    fi

    if jdk=$(find_jdk); then ok "JDK 21 at $jdk"; else miss "JDK 21: run setup --only jdk"; fi
    if [ -x "$sdk/cmdline-tools/latest/bin/sdkmanager" ]; then ok "Android command-line tools"; else miss "Android command-line tools: run setup --only cmdline-tools"; fi
    for p in $(sdk_packages); do
        if [ -d "$(pkg_dir "$p")" ]; then ok "$p"; else miss "$p: run setup --only sdk"; fi
    done
    if [ -d "$avd_home/$AVD.avd" ]; then
        ok "AVD $AVD"
        if ! grep -q "android-$EMU_API/$EMU_FLAVOR/$abi" "$avd_home/$AVD.avd/config.ini" 2>/dev/null; then
            note "AVD $AVD does not use $image_pkg; it may not match what this script tests"
        fi
    else
        miss "AVD $AVD: run setup --only avd"
    fi
    if py_ready; then ok "Python runner for the mock model ($(if [ -n "${PATCHBAY_E2E_PYTHON:-}" ]; then echo "$PATCHBAY_E2E_PYTHON"; else echo pipenv; fi))"
    else miss "pipenv (or set PATCHBAY_E2E_PYTHON): scripts/mock-llm.py runs under it"; fi

    if [ "$os" = linux ] && [ ! -w /dev/kvm ]; then miss "/dev/kvm is not writable: the emulator needs hardware acceleration"; fi
}

cmd_doctor() {
    checks
    if [ "$fail" = 0 ]; then printf '\nReady.\n'; else printf '\nNot ready: see MISSING above.\n'; exit 1; fi
}

# ---- setup ----

fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }

setup_jdk() {
    local jdk meta link sum dest tgz
    if jdk=$(find_jdk); then say "JDK 21 already at $jdk"; return; fi
    have node || die "Node is needed to read the JDK's download metadata"
    meta=$(curl -fsSL "https://api.adoptium.net/v3/assets/latest/21/hotspot?architecture=$jdk_arch&image_type=jdk&os=$os&vendor=eclipse")
    link=$(printf '%s' "$meta" | node -p 'JSON.parse(require("fs").readFileSync(0,"utf8"))[0].binary.package.link')
    sum=$(printf '%s' "$meta" | node -p 'JSON.parse(require("fs").readFileSync(0,"utf8"))[0].binary.package.checksum')
    if [ "$os" = mac ]; then dest=$HOME/Library/Java; else dest=$HOME/.local/share/java; fi
    mkdir -p "$dest"
    tgz=$(mktemp "${TMPDIR:-/tmp}/temurin.XXXXXX")
    say "downloading Temurin 21 ($link)"
    fetch "$link" "$tgz"
    [ "$(shasum -a 256 "$tgz" | cut -d' ' -f1)" = "$sum" ] || { rm -f "$tgz"; die "the JDK download does not match Adoptium's checksum"; }
    tar -xzf "$tgz" -C "$dest"
    rm -f "$tgz"
    jdk=$(find_jdk) || die "unpacked the JDK into $dest, but cannot find it there"
    say "JDK 21 at $jdk (nothing was added to your shell profile; this script finds it itself)"
}

setup_cmdline_tools() {
    local zip want xml tmp
    if [ -x "$sdk/cmdline-tools/latest/bin/sdkmanager" ]; then say "command-line tools already in $sdk"; return; fi
    zip=commandlinetools-$os-${tools_build}_latest.zip
    xml=$(curl -fsSL https://dl.google.com/android/repository/repository2-3.xml)
    want=$(printf '%s' "$xml" | ZIP=$zip node -e '
        const s = require("fs").readFileSync(0, "utf8");
        const m = s.match(new RegExp("<checksum type=\"sha1\">([0-9a-f]{40})</checksum>\\s*<url>" + process.env.ZIP.replace(/\./g, "\\.") + "</url>"));
        if (!m) process.exit(1);
        console.log(m[1]);') || die "Google does not list $zip"
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/cmdline-tools.XXXXXX")
    say "downloading $zip"
    fetch "https://dl.google.com/android/repository/$zip" "$tmp/tools.zip"
    [ "$(shasum -a 1 "$tmp/tools.zip" | cut -d' ' -f1)" = "$want" ] || { rm -rf "$tmp"; die "$zip does not match Google's checksum"; }
    unzip -q "$tmp/tools.zip" -d "$tmp"
    mkdir -p "$sdk/cmdline-tools"
    mv "$tmp/cmdline-tools" "$sdk/cmdline-tools/latest"
    rm -rf "$tmp"
    say "command-line tools in $sdk/cmdline-tools/latest"
}

setup_sdk() {
    local sdkm p missing=()
    use_env
    sdkm=$sdk/cmdline-tools/latest/bin/sdkmanager
    [ -x "$sdkm" ] || die "no command-line tools; run setup --only cmdline-tools"
    # `yes` ends by SIGPIPE when sdkmanager stops reading; that is not a failure.
    { yes || true; } | "$sdkm" --sdk_root="$sdk" --licenses >/dev/null 2>&1 || true
    for p in $(sdk_packages); do
        [ -d "$(pkg_dir "$p")" ] || missing+=("$p")
    done
    if [ "${#missing[@]}" -eq 0 ]; then say "every SDK package is installed"; return; fi
    say "installing: ${missing[*]}"
    "$sdkm" --sdk_root="$sdk" "${missing[@]}"
}

setup_avd() {
    use_env
    if [ -d "$avd_home/$AVD.avd" ]; then say "AVD $AVD already exists"; return; fi
    [ -d "$(pkg_dir "$image_pkg")" ] || die "no system image yet; run setup --only sdk first"
    say "creating AVD $AVD ($EMU_DEVICE on $image_pkg)"
    printf 'no\n' | avdmanager create avd --name "$AVD" --package "$image_pkg" --device "$EMU_DEVICE"
    [ -d "$avd_home/$AVD.avd" ] || die "avdmanager did not create $avd_home/$AVD.avd"
}

setup_rust() {
    have rustup || die "install rustup first (https://rustup.rs)"
    say "adding the Rust target $rust_target"
    rustup target add "$rust_target"
}

cmd_setup() {
    local only=
    while [ $# -gt 0 ]; do
        case "$1" in
            --only) only=${2:?--only needs a step}; shift ;;
            *) die "setup: unknown option $1" ;;
        esac
        shift
    done
    case "$only" in
        jdk) setup_jdk ;;
        cmdline-tools) setup_cmdline_tools ;;
        sdk) setup_sdk ;;
        avd) setup_avd ;;
        rust) setup_rust ;;
        "") setup_jdk; setup_cmdline_tools; setup_sdk; setup_avd; setup_rust ;;
        *) die "setup: steps are jdk, cmdline-tools, sdk, avd, rust" ;;
    esac
    if [ -z "$only" ]; then
        printf '\n'
        cmd_doctor
    fi
}

# ---- what a build is made from ----

source_id() {
    local head
    head=$(git rev-parse --short=12 HEAD)
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        printf '%s+dirty-%s\n' "$head" "$(git diff HEAD | shasum | cut -c1-8)"
    else
        printf '%s\n' "$head"
    fi
}

stamp_write() { # name version
    ensure_state
    printf 'id=%s\nrepo=%s\nversion=%s\nat=%s\n' "$(source_id)" "$top" "$2" "$(date -u +%FT%TZ)" >"$E2E_DIR/stamp/$1"
}
stamp_get() { sed -n "s/^$2=//p" "$E2E_DIR/stamp/$1" 2>/dev/null | head -n1 || true; }

# ---- building ----

target_dir() { (cd src-tauri && cargo metadata --no-deps --format-version 1 | node -p 'JSON.parse(require("fs").readFileSync(0,"utf8")).target_directory'); }
node_bin() { printf '%s/debug/patchbayd\n' "$(target_dir)"; }

find_apk() {
    local apks
    apks=$(find remote/src-tauri/gen/android/app/build/outputs/apk -name '*-debug.apk' -type f 2>/dev/null || true)
    [ -n "$apks" ] || return 1
    # shellcheck disable=SC2086
    ls -t $apks | head -n1
}

build_apk() {
    use_env
    pnpm build:remote
    (cd remote && ../node_modules/.bin/tauri android build --apk --debug --target "$tauri_target" --ci \
        --config '{"build":{"beforeBuildCommand":""}}')
    stamp_write apk "$(node -p "require('./remote/src-tauri/tauri.conf.json').version")"
}

build_node() {
    use_env
    (cd src-tauri && cargo build --bin patchbayd --no-default-features --features web)
    # The desktop UI the node serves; the same frontend as the desktop app.
    pnpm build
    stamp_write node "$(node -p "require('./package.json').version")"
}

cmd_build() {
    local what=${1:-all} rc=0 pa pn
    ensure_state
    case "$what" in
        apk) build_apk ;;
        node) build_node ;;
        all)
            say "building the APK and the daemon together (logs in $E2E_DIR/logs/build-*.log)"
            build_apk >"$E2E_DIR/logs/build-apk.log" 2>&1 &
            pa=$!
            build_node >"$E2E_DIR/logs/build-node.log" 2>&1 &
            pn=$!
            wait "$pa" || { rc=1; warn "the APK build failed; the end of its log:"; tail -n 30 "$E2E_DIR/logs/build-apk.log" >&2; }
            wait "$pn" || { rc=1; warn "the daemon build failed; the end of its log:"; tail -n 30 "$E2E_DIR/logs/build-node.log" >&2; }
            [ "$rc" = 0 ] || exit 1
            ;;
        *) die "build: apk, node or all" ;;
    esac
}

# ---- processes ----

listening_pid() { lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -n1 || true; }
wait_port() { # port seconds
    local i
    for ((i = 0; i < $2; i++)); do
        (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && return 0
        sleep 1
    done
    return 1
}
# Stop a process we started, never one that merely reuses its pid.
stop_pid() { # pid pattern
    local pid=$1 i
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || return 0
    ps -p "$pid" -o command= | grep -q -- "$2" || { warn "pid $pid is not $2; leaving it alone"; return 0; }
    kill "$pid" 2>/dev/null || true
    for ((i = 0; i < 20; i++)); do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.5; done
    kill -9 "$pid" 2>/dev/null || true
}

# Run a command in its own session, output to a log, so it outlives whatever launched this script
# (an agent, CI, a closed terminal tab). nohup only ignores hangups; it does not leave the process
# group. macOS has no setsid(1), but has perl. The pid ($!) is the command's: perl execs in place.
detach() { # logfile command [args...]
    perl -MPOSIX -e 'POSIX::setsid(); open(STDIN, "<", "/dev/null"); open(STDOUT, ">", shift @ARGV); open(STDERR, ">&", \*STDOUT); exec @ARGV or die "exec $ARGV[0]: $!\n"' "$@" &
}

mock_start() {
    if [ -n "$(listening_pid "$MOCK_PORT")" ]; then say "the mock model already answers on $MOCK_PORT"; return; fi
    py_ready || die "pipenv is needed for scripts/mock-llm.py (or set PATCHBAY_E2E_PYTHON)"
    ensure_state
    if [ -z "${PATCHBAY_E2E_PYTHON:-}" ]; then
        [ -f "$E2E_DIR/Pipfile" ] || printf '[[source]]\nurl = "https://pypi.org/simple"\nverify_ssl = true\nname = "pypi"\n\n[packages]\n\n[dev-packages]\n' >"$E2E_DIR/Pipfile"
        export PIPENV_PIPFILE=$E2E_DIR/Pipfile PIPENV_VERBOSITY=-1 PIPENV_IGNORE_VIRTUALENVS=1
        detach "$E2E_DIR/logs/mock.log" pipenv run python "$top/scripts/mock-llm.py" --port "$MOCK_PORT"
    else
        detach "$E2E_DIR/logs/mock.log" "$PATCHBAY_E2E_PYTHON" "$top/scripts/mock-llm.py" --port "$MOCK_PORT"
    fi
    say "starting the mock model on $MOCK_PORT"
    wait_port "$MOCK_PORT" 120 || { tail -n 20 "$E2E_DIR/logs/mock.log" >&2; die "the mock model did not start"; }
}
mock_stop() { stop_pid "$(listening_pid "$MOCK_PORT")" mock-llm.py; }

node_running() {
    local pid
    pid=$(cat "$E2E_DIR/node.pid" 2>/dev/null || true)
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && ps -p "$pid" -o command= | grep -q patchbayd
}

node_start() {
    local bin
    ensure_state
    if node_running; then
        [ "$(stamp_get node repo)" = "$top" ] || die "a node from $(stamp_get node repo) is already running; run down first"
        say "the node is already running (built from $(stamp_get node id))"
        return
    fi
    bin=$(node_bin)
    [ -x "$bin" ] || die "no daemon at $bin; run: scripts/android-e2e.sh build node"
    [ -f dist/index.html ] || die "no built desktop UI in dist/; run: scripts/android-e2e.sh build node"
    [ -z "$(listening_pid "$WEB_PORT")" ] || die "port $WEB_PORT is in use; set PATCHBAY_E2E_WEB_PORT"
    [ -z "$(listening_pid "$MESH_PORT")" ] || die "port $MESH_PORT is in use; set PATCHBAY_E2E_MESH_PORT"
    mkdir -p "$E2E_DIR/node"
    chmod 700 "$E2E_DIR/node"
    say "starting the node (built from $(stamp_get node id)) on web port $WEB_PORT, mesh port $MESH_PORT"
    # A HOME with no login keychain: on macOS the node then cannot reach the user's keychain, so it
    # keeps its mesh secrets in its data folder instead of asking for a keychain login on every
    # run (a fresh debug build is a new app to the keychain, so "Always Allow" never sticks).
    mkdir -p "$E2E_DIR/home"
    detach "$E2E_DIR/logs/node.log" env HOME="$E2E_DIR/home" PATCHBAY_LOG="${PATCHBAY_LOG:-info}" \
        "$bin" --data-dir "$E2E_DIR/node" --listen "127.0.0.1:$WEB_PORT" --ui-dir "$top/dist"
    echo $! >"$E2E_DIR/node.pid"
    wait_port "$WEB_PORT" 60 || { tail -n 20 "$E2E_DIR/logs/node.log" >&2; die "the node did not start"; }
}
node_stop() {
    stop_pid "$(cat "$E2E_DIR/node.pid" 2>/dev/null || true)" patchbayd
    rm -f "$E2E_DIR/node.pid"
}

passphrase() {
    local f=$E2E_DIR/passphrase
    ensure_state
    if [ ! -s "$f" ]; then (umask 077; od -An -N16 -tx1 /dev/urandom | tr -d ' \n' >"$f"); fi
    cat "$f"
}

make_project() {
    local p=$E2E_DIR/project
    if [ ! -d "$p/.git" ]; then
        mkdir -p "$p"
        git -C "$p" init -q
        printf '# E2E project\n' >"$p/README.md"
        git -C "$p" add README.md
        git -C "$p" -c user.name=e2e -c user.email=e2e@localhost commit -q -m "Start the E2E project"
    fi
    printf '%s\n' "$p"
}

daemon_flags() { printf -- '--ws ws://127.0.0.1:%s/ws --token-file %s' "$WEB_PORT" "$E2E_DIR/node/web-token"; }

node_seed() {
    local project
    project=$(make_project)
    say "making the mesh and what the phone will show"
    # shellcheck disable=SC2046
    E2E_PASSPHRASE=$(passphrase) node "$helpers/daemon.mjs" seed $(daemon_flags) \
        --name "$DESKTOP_NAME" --listen "127.0.0.1:$MESH_PORT" \
        --mock-url "http://127.0.0.1:$MOCK_PORT/v1" --project "$project" >"$E2E_DIR/seed.json"
}

cmd_node() {
    case "${1:-start}" in
        start) use_env; mock_start; node_start; node_seed ;;
        stop) node_stop; mock_stop ;;
        *) die "node: start or stop" ;;
    esac
}

# ---- the emulator ----

emulator_serials() { adb devices | awk '/^emulator-[0-9]+[[:space:]]+device/{print $1}'; }
emulator_serial() {
    local s
    for s in $(emulator_serials); do
        if [ "$(adb -s "$s" emu avd name 2>/dev/null | head -n1 | tr -d '\r')" = "$AVD" ]; then printf '%s\n' "$s"; return 0; fi
    done
    return 1
}

emulator_start() { # headless cold
    local serial i args
    if serial=$(emulator_serial); then
        say "the emulator $AVD is already running as $serial"
    else
        [ -x "$sdk/emulator/emulator" ] || die "no emulator; run setup --only sdk"
        [ -d "$avd_home/$AVD.avd" ] || die "no AVD $AVD; run setup --only avd"
        ensure_state
        args=(-avd "$AVD" -netdelay none -netspeed full -no-boot-anim)
        if [ "$1" = 1 ]; then args+=(-no-window -no-audio -gpu swiftshader_indirect); fi
        if [ "$2" = 1 ]; then args+=(-no-snapshot-load); fi
        say "starting the emulator $AVD"
        detach "$E2E_DIR/logs/emulator.log" "$sdk/emulator/emulator" "${args[@]}"
        echo $! >"$E2E_DIR/emulator.pid"
        for ((i = 0; i < 150; i++)); do
            if serial=$(emulator_serial); then break; fi
            sleep 2
        done
        [ -n "${serial:-}" ] || { tail -n 20 "$E2E_DIR/logs/emulator.log" >&2; die "the emulator did not appear in adb"; }
        for ((i = 0; i < 120; i++)); do
            [ "$(adb -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && break
            sleep 2
        done
        [ "$(adb -s "$serial" shell getprop sys.boot_completed | tr -d '\r')" = 1 ] || die "Android did not finish booting"
    fi
    export ANDROID_SERIAL=$serial
    # The emulator's touch screen reports a stylus, which turns Gboard into a handwriting toolbar and
    # keeps a typing field from taking the Mac's keys. Animations off make the app steadier to drive.
    adb shell settings put secure stylus_handwriting_enabled 0
    adb shell settings put global window_animation_scale 0
    adb shell settings put global transition_animation_scale 0
    adb shell settings put global animator_duration_scale 0
}
emulator_stop() {
    local serial i
    if serial=$(emulator_serial); then
        adb -s "$serial" emu kill >/dev/null 2>&1 || true
        # It leaves adb a few seconds later; a start right after must not mistake it for a live one.
        for ((i = 0; i < 30; i++)); do
            emulator_serial >/dev/null || break
            sleep 1
        done
    fi
    rm -f "$E2E_DIR/emulator.pid"
}

use_serial() {
    local serial
    serial=$(emulator_serial) || die "the emulator $AVD is not running; run: scripts/android-e2e.sh emulator start"
    export ANDROID_SERIAL=$serial
}

cmd_emulator() {
    local act=${1:-start} headless=0 cold=0
    [ $# -gt 0 ] && shift
    while [ $# -gt 0 ]; do
        case "$1" in
            --headless) headless=1 ;;
            --cold) cold=1 ;;
            *) die "emulator: unknown option $1" ;;
        esac
        shift
    done
    use_env
    case "$act" in
        start) emulator_start "$headless" "$cold" ;;
        stop) emulator_stop ;;
        *) die "emulator: start or stop" ;;
    esac
}

# ---- the phone ----

cmd_install() {
    local fresh=${1:-}
    local apk
    use_env
    use_serial
    apk=$(find_apk) || die "no debug APK; run: scripts/android-e2e.sh build apk"
    say "installing $apk (built from $(stamp_get apk id))"
    adb install -r -g "$apk" >/dev/null || die "install failed; if the signature differs from a build made elsewhere, run: adb uninstall $APP_ID"
    adb shell am force-stop "$APP_ID"
    if [ "$fresh" = --fresh ]; then adb shell pm clear "$APP_ID" >/dev/null; fi
}

cmd_join() {
    use_env
    use_serial
    [ -s "$E2E_DIR/seed.json" ] || die "no mesh yet; run: scripts/android-e2e.sh node start"
    # Otherwise Android 13+ asks "Allow notifications?" over the app and eats taps (and screenshots).
    adb shell pm grant "$APP_ID" android.permission.POST_NOTIFICATIONS >/dev/null 2>&1 || true
    adb shell am start -n "$APP_ID/.MainActivity" >/dev/null
    say "joining as $PHONE_NAME through 10.0.2.2:$MESH_PORT (the emulator's name for this computer)"
    E2E_PASSPHRASE=$(passphrase) node "$helpers/phone.mjs" join --address "10.0.2.2:$MESH_PORT" --name "$PHONE_NAME"
    # shellcheck disable=SC2046
    node "$helpers/daemon.mjs" approve $(daemon_flags) --phone "$PHONE_NAME"
}

cmd_verify() {
    use_env
    use_serial
    [ -s "$E2E_DIR/seed.json" ] || die "no mesh yet; run: scripts/android-e2e.sh node start"
    node "$helpers/phone.mjs" verify --seed "$E2E_DIR/seed.json"
}

cmd_shot() {
    local out
    use_env
    use_serial
    ensure_state
    out=$E2E_DIR/shots/$(date +%Y%m%dT%H%M%S).png
    adb exec-out screencap -p >"$out"
    printf '%s\n' "$out"
}

cmd_logs() {
    use_env
    use_serial
    printf '%s\n' '--- the app (adb logcat) ---'
    adb logcat -d --pid="$(adb shell pidof "$APP_ID" | tr -d '\r' | awk '{print $1}')" | tail -n 80 || true
    printf '%s\n' '--- the node ---'
    tail -n 40 "$E2E_DIR/logs/node.log" 2>/dev/null || true
}

# ---- the whole run ----

cmd_up() {
    local fresh=0 headless=0 build=1 cold=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --fresh) fresh=1 ;;
            --headless) headless=1 ;;
            --no-build) build=0 ;;
            --cold) cold=1 ;;
            *) die "up: unknown option $1" ;;
        esac
        shift
    done
    checks >&2
    [ "$fail" = 0 ] || die "not ready; fix what is MISSING above (setup does most of it)"
    use_env

    if [ "$fresh" = 1 ]; then
        say "starting over: a new mesh and clean app data"
        node_stop
        rm -rf "$E2E_DIR/node" "$E2E_DIR/passphrase" "$E2E_DIR/seed.json"
        # The projects the sessions work in too: a file an earlier run's edit made would turn this
        # run's `!edit` into "file has not been read yet" instead of a permission request.
        rm -rf "${E2E_DIR:?}/project" "${E2E_DIR:?}/alpha-app" "${E2E_DIR:?}/beta-lib"
    fi
    if [ "$build" = 1 ]; then
        node_stop
        cmd_build all
    else
        [ -f "$E2E_DIR/stamp/apk" ] && [ -f "$E2E_DIR/stamp/node" ] || die "--no-build, but nothing was built yet"
        [ "$(stamp_get apk id)" = "$(source_id)" ] && [ "$(stamp_get node id)" = "$(source_id)" ] ||
            warn "the builds are of $(stamp_get apk id) and $(stamp_get node id), not of what is checked out now ($(source_id))"
    fi

    mock_start
    node_start
    node_seed
    emulator_start "$headless" "$cold"
    if [ "$build" = 1 ] || ! adb shell pm list packages "$APP_ID" | grep -q "$APP_ID"; then
        if [ "$fresh" = 1 ]; then cmd_install --fresh; else cmd_install; fi
    elif [ "$fresh" = 1 ]; then
        adb shell pm clear "$APP_ID" >/dev/null
    fi
    cmd_join
    cmd_verify
    cmd_shot >/dev/null
    printf '\nThe phone and the node are the same source, %s.\n' "$(stamp_get node id)"
    cmd_status
}

cmd_test() {
    local only="" up=1 keep=0 build_flag=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --only) only=${2:?--only needs a list}; shift ;;
            --no-up) up=0 ;;
            --keep) keep=1 ;;
            --no-build) build_flag=--no-build ;;
            --list) node "$helpers/scenarios.mjs" --list; return ;;
            *) die "test: unknown option $1" ;;
        esac
        shift
    done
    local rc=0
    if [ "$up" = 1 ]; then
        # shellcheck disable=SC2086
        cmd_up --fresh --headless $build_flag || { cmd_down --emulator; die "the stack did not come up; no scenario ran"; }
    fi
    use_env
    use_serial
    # shellcheck disable=SC2046
    node "$helpers/daemon.mjs" more-sessions $(daemon_flags) --root "$E2E_DIR" --mock-url "http://127.0.0.1:$MOCK_PORT/v1"
    node "$helpers/scenarios.mjs" --seed "$E2E_DIR/seed.json" --src "$top" ${only:+--only "$only"} || rc=$?
    if [ "$keep" = 0 ]; then cmd_down --emulator; fi
    return "$rc"
}

cmd_status() {
    local serial token apk_id node_id installed
    ensure_state
    printf 'Checkout     %s (%s)\n' "$top" "$(source_id)"
    apk_id=$(stamp_get apk id)
    node_id=$(stamp_get node id)
    printf 'APK built    %s\n' "${apk_id:-never}${apk_id:+ at $(stamp_get apk at)}"
    printf 'Node built   %s\n' "${node_id:-never}${node_id:+ at $(stamp_get node at)}"
    if [ -n "$apk_id" ] && [ -n "$node_id" ]; then
        if [ "$apk_id" = "$node_id" ]; then printf 'Same source  yes\n'; else printf 'Same source  NO: rebuild with up\n'; fi
    fi
    if node_running; then
        token=$(cat "$E2E_DIR/node/web-token" 2>/dev/null || true)
        printf 'Node         running, desktop UI http://127.0.0.1:%s/#token=%s\n' "$WEB_PORT" "$token"
        # shellcheck disable=SC2046
        printf 'Mesh         %s\n' "$(node "$helpers/daemon.mjs" status $(daemon_flags) 2>&1 || true)"
    else
        printf 'Node         stopped\n'
    fi
    if [ -n "$(listening_pid "$MOCK_PORT")" ]; then printf 'Mock model   running on %s\n' "$MOCK_PORT"; else printf 'Mock model   stopped\n'; fi
    if have adb || [ -x "$sdk/platform-tools/adb" ]; then
        PATH="$sdk/platform-tools:$PATH"
        if serial=$(emulator_serial 2>/dev/null); then
            installed=$(adb -s "$serial" shell dumpsys package "$APP_ID" 2>/dev/null | sed -n 's/.*versionName=//p' | head -n1 | tr -d '\r' || true)
            printf 'Emulator     %s running as %s, app %s\n' "$AVD" "$serial" "${installed:-not installed}"
        else
            printf 'Emulator     %s not running\n' "$AVD"
        fi
    fi
}

cmd_down() {
    node_stop
    mock_stop
    if [ "${1:-}" = --emulator ]; then
        use_env
        emulator_stop
    fi
    say "stopped. The mesh, chat and session are kept in $E2E_DIR/node; up --fresh starts over."
}

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

cmd=${1:-help}
[ $# -gt 0 ] && shift
case "$cmd" in
    doctor) cmd_doctor ;;
    setup) cmd_setup "$@" ;;
    build) cmd_build "$@" ;;
    emulator) cmd_emulator "$@" ;;
    node) cmd_node "$@" ;;
    install) cmd_install "$@" ;;
    join) cmd_join ;;
    verify) cmd_verify ;;
    shot) cmd_shot ;;
    logs) cmd_logs ;;
    up) cmd_up "$@" ;;
    test) cmd_test "$@" ;;
    status) cmd_status ;;
    down) cmd_down "$@" ;;
    help | -h | --help) usage ;;
    *) usage >&2; exit 2 ;;
esac
