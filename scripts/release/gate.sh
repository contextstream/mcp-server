#!/usr/bin/env bash
# The release gate at the release commit (.depot/workflows/release.yml, job gate), in two parts that run side by side:
#
#   scripts/release/gate.sh rust     fmt, clippy, the workspace tests, the debug build and its account-connection smoke
#   scripts/release/gate.sh checks   cargo audit and cargo deny, the release contract, the public boundary, every script
#                                    test, the adoption and grounding tests, and the npm launcher tests
#
# Every step runs even after a failure; the script fails when any step failed. Tool downloads are pinned by sha256.
set -uo pipefail
part=${1:?rust or checks}
cd "$(dirname "$0")/../.."

AUDIT_URL=https://github.com/rustsec/rustsec/releases/download/cargo-audit/v0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz
AUDIT_SHA256=7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428
DENY_URL=https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2/cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz
DENY_SHA256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f

failed=()
step() {
  local name=$1 start=$SECONDS; shift
  echo "::group::$name"
  if "$@"; then echo "::endgroup::"; echo "gate: $name ok ($((SECONDS - start)) s)"
  else echo "::endgroup::"; echo "gate: $name FAILED ($((SECONDS - start)) s)"; failed+=("$name"); fi
}
tool() { # URL SHA256 NAME: the pinned binary NAME from a release archive
  local dir; dir=$(mktemp -d)
  curl -fsSL --retry 4 --max-time 300 -o "$dir/archive.tgz" "$1" && echo "$2  $dir/archive.tgz" | sha256sum -c --quiet - \
    && tar -xzf "$dir/archive.tgz" -C "$dir" && find "$dir" -type f -name "$3" | head -1
}

export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
version=$(python3 -c 'import json; print(json.load(open("package.json"))["version"])')

case "$part" in
  rust)
    step fmt cargo fmt --all --check
    step clippy cargo clippy --locked --workspace --all-targets -- -D warnings
    step test cargo test --locked --workspace --all-targets
    step build cargo build --locked --package mcp-server --bin contextstream-mcp
    step smoke python3 .github/scripts/account_connection_smoke.py target/debug/contextstream-mcp
    ;;
  checks)
    audit=$(tool "$AUDIT_URL" "$AUDIT_SHA256" cargo-audit)
    deny=$(tool "$DENY_URL" "$DENY_SHA256" cargo-deny)
    step audit "${audit:-false}" audit --deny warnings
    step deny "${deny:-false}" --locked check
    step release_contract python3 .github/scripts/release_contract.py metadata .
    step resolve_version python3 .github/scripts/release_contract.py resolve-version . push "refs/tags/v$version"
    step public_boundary python3 .github/scripts/public_boundary.py .
    for t in test_public_boundary test_release_contract test_workflows test_dco_check; do
      step "$t" python3 ".github/scripts/$t.py"
    done
    step release_scripts python3 -m unittest discover -s scripts/release -p 'test_*.py'
    step release_shell bash -n scripts/release/build.sh scripts/release/gate.sh
    step adoption_tests bash -c 'cd testing/adoption && python3 -m unittest discover -s . -p "test_*.py"'
    for t in testing/grounding/test_*.py; do
      step "grounding_$(basename "$t" .py)" bash -c "cd testing/grounding && python3 $(basename "$t")"
    done
    step npm_test npm test
    ;;
  *) echo "gate.sh: unknown part $part" >&2; exit 2 ;;
esac

if [ ${#failed[@]} -gt 0 ]; then
  echo "gate: $part FAILED: ${failed[*]}"
  exit 1
fi
echo "gate: $part GATE_OK"
