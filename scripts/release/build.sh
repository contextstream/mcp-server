#!/usr/bin/env bash
# Build and check one release binary on a Depot CI runner (.depot/workflows/release.yml, job build).
#
#   scripts/release/build.sh <artifact> <version>
#
# Writes out/<artifact> in the repository root. The workflow picks the runner: linux-arm64 builds natively on an
# arm64 runner, everything else on x86_64. Windows builds with cargo-xwin (MSVC CRT and SDK, license accepted by
# the owner) and is run once under Wine. The darwin binaries build with cargo-zigbuild against the link stubs in
# scripts/release/macos-link-stubs (no macOS runner exists in Depot CI) and are checked structurally, since Linux
# cannot run them. Every tool download is pinned by sha256. SOURCE_DATE_EPOCH comes from the workflow.
set -euo pipefail

artifact=${1:?artifact}
version=${2:?version}
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

ZIG_VERSION=0.13.0
ZIG_SHA256=d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea
ZIG_DIR=zig-linux-x86_64-$ZIG_VERSION
ZIGBUILD_VERSION=v0.23.4
ZIGBUILD_SHA256=9e3cf73485edbd45905c8aadbc0fdf869c7ddc3848f0c898229f2680db52e44b
XWIN_VERSION=v0.23.1
XWIN_SHA256=c492c6dfb7e5ac0eee586b796e0fc950ba13077e7f0bbb046f445d71790d5360
GLIBC_FLOOR=2.39

features=()
exe=contextstream-mcp
case "$artifact" in
  contextstream-mcp-linux-x64) target=x86_64-unknown-linux-gnu; kind=linux ;;
  contextstream-mcp-linux-x64-remote-acceleration)
    target=x86_64-unknown-linux-gnu; kind=linux; features=(--features remote-acceleration) ;;
  contextstream-mcp-linux-arm64) target=aarch64-unknown-linux-gnu; kind=linux ;;
  contextstream-mcp-win-x64.exe) target=x86_64-pc-windows-msvc; kind=windows; exe=contextstream-mcp.exe ;;
  contextstream-mcp-darwin-arm64) target=aarch64-apple-darwin; kind=darwin; export MACOSX_DEPLOYMENT_TARGET=11.0 ;;
  contextstream-mcp-darwin-x64) target=x86_64-apple-darwin; kind=darwin; export MACOSX_DEPLOYMENT_TARGET=10.12 ;;
  *) echo "build.sh: unknown artifact $artifact" >&2; exit 2 ;;
esac

log() { echo "build.sh: $*"; }
tools=$(mktemp -d)
trap 'rm -rf "$tools"' EXIT

# fetch URL SHA256 FILE: download and check the pinned digest.
fetch() {
  curl -fsSL --retry 4 --retry-delay 3 --max-time 300 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet -
}

rustup target add --toolchain "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)" "$target" >/dev/null

case "$kind" in
  linux)
    case "$(uname -m):$target" in
      x86_64:x86_64-*|aarch64:aarch64-*) ;;
      *) echo "build.sh: $target must build on its own architecture (runner is $(uname -m))" >&2; exit 2 ;;
    esac
    cargo build --locked --release --package mcp-server --target "$target" "${features[@]}"
    ;;
  windows)
    command -v lld-link >/dev/null && { command -v wine64 >/dev/null || [ -x /usr/lib/wine/wine64 ]; } || {
      sudo apt-get update -qq
      sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends clang lld llvm nasm wine64 >/dev/null
    }
    fetch "https://github.com/messense/cargo-xwin/releases/download/$XWIN_VERSION/cargo-xwin-$XWIN_VERSION.x86_64-unknown-linux-musl.tar.gz" \
      "$XWIN_SHA256" "$tools/xwin.tgz"
    tar -xzf "$tools/xwin.tgz" -C "$tools"
    export PATH="$tools:$PATH" XWIN_ACCEPT_LICENSE=1
    cargo xwin build --locked --release --package mcp-server --target "$target"
    ;;
  darwin)
    fetch "https://ziglang.org/download/$ZIG_VERSION/$ZIG_DIR.tar.xz" "$ZIG_SHA256" "$tools/zig.tar.xz"
    tar -xJf "$tools/zig.tar.xz" -C "$tools"
    fetch "https://github.com/rust-cross/cargo-zigbuild/releases/download/$ZIGBUILD_VERSION/cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz" \
      "$ZIGBUILD_SHA256" "$tools/zigbuild.tar.xz"
    tar -xJf "$tools/zigbuild.tar.xz" -C "$tools"
    zigbuild=$(find "$tools" -type f -name cargo-zigbuild | head -1)
    install -m 0755 "$zigbuild" "$tools/cargo-zigbuild"
    export PATH="$tools:$tools/$ZIG_DIR:$PATH"
    stubs=$root/scripts/release/macos-link-stubs
    # RUSTFLAGS would replace the per-target flags, so the build uses only these.
    unset RUSTFLAGS
    flags="-C link-arg=-F$stubs/System/Library/Frameworks -C link-arg=-L$stubs/usr/lib"
    upper=$(echo "$target" | tr 'a-z-' 'A-Z_')
    export "CARGO_TARGET_${upper}_RUSTFLAGS=$flags"
    # Header shims for C dependencies (jemalloc), added to Zig's own macOS headers (Zig does not honour -isystem or
    # -idirafter for them). Only headers Zig lacks are added; an existing one is never replaced.
    zig_macos=$tools/$ZIG_DIR/lib/libc/include/any-macos-any
    [ -d "$zig_macos/sys" ] || { echo "build.sh: zig $ZIG_VERSION has no $zig_macos/sys" >&2; exit 1; }
    (cd "$stubs/include" && find . -type f -name '*.h') | while read -r header; do
      [ -e "$zig_macos/$header" ] || install -D -m 0644 "$stubs/include/$header" "$zig_macos/$header"
    done
    # The compile-cache server runs compilers with its own environment: restart it so it sees Zig on PATH.
    if [ "${RUSTC_WRAPPER:-}" = sccache ]; then sccache --stop-server >/dev/null 2>&1 || true; sccache --start-server >/dev/null; fi
    log "zig $(zig version), $(cargo-zigbuild --version), deployment target $MACOSX_DEPLOYMENT_TARGET"
    cargo zigbuild --locked --release --package mcp-server --target "$target"
    ;;
esac

binary=target/$target/release/$exe
test -s "$binary"
mkdir -p out
install -m 0755 "$binary" "out/$artifact"

case "$kind" in
  linux)
    python3 .github/scripts/release_contract.py verify-binary "out/$artifact" "$version"
    newest=$(objdump -T "out/$artifact" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
    [ "$(printf '%s\n%s\n' "$newest" "$GLIBC_FLOOR" | sort -V | tail -1)" = "$GLIBC_FLOOR" ] || {
      echo "build.sh: $artifact needs glibc $newest, above the $GLIBC_FLOOR floor" >&2; exit 1; }
    log "$artifact needs glibc $newest at most"
    ;;
  windows)
    # Ubuntu 24.04's wine64 package leaves its 64-bit loader off PATH, and its `wine` may need the 32-bit one.
    export WINEDEBUG=-all WINEPREFIX=$tools/wine
    shown=""
    for wine in /usr/lib/wine/wine64 wine64 wine; do
      command -v "$wine" >/dev/null || continue
      shown=$("$wine" "out/$artifact" --version 2>&1 | tr -d '\r' | tail -1) || true
      [ "$shown" = "contextstream-mcp $version" ] && break
      log "$wine printed [$shown]"
    done
    [ "$shown" = "contextstream-mcp $version" ] || { echo "build.sh: wine printed [$shown]" >&2; exit 1; }
    log "wine: $shown"
    log "DLLs: $(objdump -p "out/$artifact" | awk '/DLL Name/{print $3}' | sort | tr '\n' ' ')"
    ;;
  darwin)
    python3 scripts/release/mcp_release.py macho-check "out/$artifact" --version "$version"
    ;;
esac
log "$artifact $(sha256sum "out/$artifact" | cut -d' ' -f1) OK"
