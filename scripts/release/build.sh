#!/usr/bin/env bash
# Build and check one release binary on a Depot CI runner (.depot/workflows/release.yml, job build).
#
#   scripts/release/build.sh <artifact> <version>
#
# Writes out/<artifact> in the repository root. The workflow picks the runner: linux-arm64 builds natively on an
# arm64 runner, everything else on x86_64. Windows builds with cargo-xwin (MSVC CRT and SDK, license accepted by
# the owner) and is run once under Wine. The darwin binaries build with clang and ld64.lld against an SDK root made
# from Zig's macOS libc headers and the stubs in scripts/release/macos-link-stubs (no macOS runner exists in Depot CI)
# and are checked structurally, since Linux cannot run them. Every tool download is pinned by sha256. SOURCE_DATE_EPOCH comes from the workflow.
set -euo pipefail

artifact=${1:?artifact}
version=${2:?version}
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

ZIG_VERSION=0.14.1
ZIG_SHA256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
ZIG_DIR=zig-x86_64-linux-$ZIG_VERSION
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
# A fixed path: the darwin build's linker and SDK paths are part of every compile command, so a fresh temporary
# directory per run would miss the compile cache for every crate.
tools=$HOME/.cache/mcp-release-build-tools
rm -rf "$tools" && mkdir -p "$tools"
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
    # A macOS SDK root made without Apple's SDK: the libc headers and libSystem stub that Zig ships (we use no other
    # part of Zig), the framework and libiconv stubs of scripts/release/macos-link-stubs, and its header shims for what
    # Zig lacks. Stock LLVM from Ubuntu compiles the C dependencies (clang) and links (ld64.lld) against it.
    compgen -G '/usr/lib/llvm-*/bin/ld64.lld' >/dev/null && compgen -G '/usr/lib/llvm-*/bin/llvm-ar' >/dev/null || {
      sudo apt-get update -qq
      sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends clang lld llvm >/dev/null
    }
    # Ubuntu installs ld64.lld only under its LLVM directory; clang from the same directory finds it.
    llvm_bin=$(dirname "$(ls /usr/lib/llvm-*/bin/ld64.lld | sort -V | tail -1)")
    export PATH="$llvm_bin:$PATH"
    fetch "https://ziglang.org/download/$ZIG_VERSION/$ZIG_DIR.tar.xz" "$ZIG_SHA256" "$tools/zig.tar.xz"
    tar -xJf "$tools/zig.tar.xz" -C "$tools"
    zig_libc=$tools/$ZIG_DIR/lib/libc
    stubs=$root/scripts/release/macos-link-stubs
    sdk=$tools/MacOSX.sdk
    mkdir -p "$sdk/usr/include" "$sdk/usr/lib" "$sdk/System/Library/Frameworks"
    [ -d "$zig_libc/include/any-macos-any/sys" ] || { echo "build.sh: zig $ZIG_VERSION has no macOS headers" >&2; exit 1; }
    cp -R "$zig_libc/include/any-macos-any/." "$sdk/usr/include/"
    arch_headers=$zig_libc/include/${target%%-*}-macos-none
    [ ! -d "$arch_headers" ] || cp -R "$arch_headers/." "$sdk/usr/include/"
    libsystem=$(find "$zig_libc" -name libSystem.tbd | head -1)
    [ -s "$libsystem" ] || { echo "build.sh: zig $ZIG_VERSION has no libSystem.tbd" >&2; exit 1; }
    cp "$libsystem" "$sdk/usr/lib/libSystem.tbd"
    ln -s libSystem.tbd "$sdk/usr/lib/libc.tbd"
    ln -s libSystem.tbd "$sdk/usr/lib/libm.tbd"
    cp "$stubs/usr/lib/libiconv.tbd" "$sdk/usr/lib/libiconv.tbd"
    cp -R "$stubs/System/Library/Frameworks/." "$sdk/System/Library/Frameworks/"
    (cd "$stubs/include" && find . -type f -name '*.h') | while read -r header; do
      [ -e "$sdk/usr/include/$header" ] || install -D -m 0644 "$stubs/include/$header" "$sdk/usr/include/$header"
    done
    printf '{"CanonicalName": "macosx14.0", "DisplayName": "macOS 14.0", "Version": "14.0", "MaximumDeploymentTarget": "14.0.99"}\n' \
      > "$sdk/SDKSettings.json"
    triple=${target%%-*}-apple-macos$MACOSX_DEPLOYMENT_TARGET
    triple=${triple/aarch64-/arm64-}
    # One driver for compiling and linking (configure scripts link test programs with CC): clang, with ld64.lld.
    printf '#!/bin/sh\nexec clang --target=%s -isysroot %s -fuse-ld=lld -Wno-unused-command-line-argument "$@"\n' \
      "$triple" "$sdk" > "$tools/darwin-cc"
    chmod 0755 "$tools/darwin-cc"
    underscored=${target//-/_}
    upper=$(echo "$underscored" | tr 'a-z' 'A-Z')
    export SDKROOT=$sdk "CC_$underscored=$tools/darwin-cc" "AR_$underscored=llvm-ar" "RANLIB_$underscored=llvm-ranlib" \
      "CARGO_TARGET_${upper}_LINKER=$tools/darwin-cc"
    unset RUSTFLAGS
    log "$(clang --version | head -1), $(ld64.lld --version | head -1), $triple"
    cargo build --locked --release --package mcp-server --target "$target"
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
