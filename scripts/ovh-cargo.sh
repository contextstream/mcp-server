#!/usr/bin/env bash
# Run a cargo command for the CURRENT working tree on the OVH dev host.
#
# Builds never run on the Mac. This ships the working tree as a git patch over
# ssh (never rsync): the remote worktree is reset to the same upstream base
# commit (merge-base with origin/main), the patch of committed-but-unpushed,
# uncommitted, and untracked changes is applied, then cargo runs there with a
# per-worktree target directory.
#
# Usage:
#   scripts/ovh-cargo.sh check --workspace
#   scripts/ovh-cargo.sh test -p mcp-tools lessons -- --nocapture
#   scripts/ovh-cargo.sh clippy --workspace --all-targets -- -D warnings
#   scripts/ovh-cargo.sh fmt --check
#
# Env: OVH_HOST (default ovh-dev),
#      OVH_REMOTE_DIR (default ~/dev/maker/mcp-server-wt-parity),
#      OVH_TARGET_DIR (default ~/dev/maker/mcp-server-wt-parity/target),
#      OVH_EXTRA_ENV (extra `KEY=value` pairs exported before cargo).
set -euo pipefail

HOST=${OVH_HOST:-ovh-dev}
REMOTE_DIR=${OVH_REMOTE_DIR:-'$HOME/dev/maker/mcp-server-wt-parity'}
TARGET_DIR=${OVH_TARGET_DIR:-'$HOME/dev/maker/mcp-server-wt-parity/target'}
EXTRA_ENV=${OVH_EXTRA_ENV:-}

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "ovh-cargo: run from inside the mcp-server checkout" >&2
  exit 2
fi
if ! git rev-parse --verify -q origin/main >/dev/null; then
  echo "ovh-cargo: origin/main is unknown locally; run 'git fetch origin' first" >&2
  exit 2
fi
if [ "$#" -eq 0 ]; then
  echo "ovh-cargo: pass the cargo subcommand and arguments" >&2
  exit 2
fi

BASE=$(git merge-base HEAD origin/main)
PATCH=$(mktemp -t ovh-cargo-patch.XXXXXX)
UNTRACKED=$(mktemp -t ovh-cargo-untracked.XXXXXX)
trap 'rm -f "$PATCH" "$UNTRACKED"' EXIT

git ls-files --others --exclude-standard -z > "$UNTRACKED"
if [ -s "$UNTRACKED" ]; then
  xargs -0 git add -N -- < "$UNTRACKED"
fi
git diff --binary --no-color "$BASE" > "$PATCH"
if [ -s "$UNTRACKED" ]; then
  xargs -0 git reset -q -- < "$UNTRACKED"
fi

CARGO_ARGS=$(printf '%q ' "$@")
echo "ovh-cargo: base $(git rev-parse --short "$BASE"), patch $(wc -c < "$PATCH" | tr -d ' ') bytes, host $HOST: cargo $*" >&2

# shellcheck disable=SC2029
ssh -o BatchMode=yes "$HOST" "export PATH=\$HOME/.cargo/bin:\$PATH; set -e
REMOTE_DIR=$REMOTE_DIR
MAIN_DIR=\$HOME/dev/maker/mcp-server
if [ ! -d \"\$MAIN_DIR/.git\" ]; then git clone -q git@github.com:contextstream/mcp-server.git \"\$MAIN_DIR\"; fi
git -C \"\$MAIN_DIR\" fetch -q origin
if [ ! -e \"\$REMOTE_DIR/.git\" ]; then git -C \"\$MAIN_DIR\" worktree add -q --detach \"\$REMOTE_DIR\" $BASE; fi
cd \"\$REMOTE_DIR\"
git checkout -q --detach $BASE
git reset -q --hard
git clean -qfd -e target
git apply --binary --whitespace=nowarn --allow-empty -
echo \"== applied: \$(git status --short | wc -l | tr -d ' ') paths differ from base\" >&2
git status --short | sed 's/^/==   /' >&2
export CARGO_TARGET_DIR=$TARGET_DIR
export CARGO_INCREMENTAL=0
$EXTRA_ENV
cargo $CARGO_ARGS" < "$PATCH"
