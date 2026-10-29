#!/usr/bin/env bash
# Regenerate and publish github.com/contextstream/antigravity-plugin.
#
# The repository holds exactly what `contextstream-mcp export-antigravity-plugin`
# writes (the code `setup` uses for the plugin on a person's machine), plus the
# logo and the license from this repository. Run it after a release that
# changes the plugin's rules, skill, hooks or manifest, with a build that has
# the command:
#
#   CONTEXTSTREAM_MCP_BIN=/path/to/contextstream-mcp scripts/release/publish_antigravity_plugin.sh
#   scripts/release/publish_antigravity_plugin.sh --dry-run      # show the change, push nothing
#
# `plugin.json` `version` is PUBLISHED_VERSION in
# crates/mcp-server/src/setup/antigravity_plugin.rs: raise it when the published
# files change in a way people should see.
#
# Needs: gh (signed in, with push access to the repository), git, and the binary.
# With `agy` on the PATH it also runs `agy plugin validate --strict` first.

set -euo pipefail

target="contextstream/antigravity-plugin"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary="${CONTEXTSTREAM_MCP_BIN:-contextstream-mcp}"
dry_run=0
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

if ! "$binary" export-antigravity-plugin --json >/dev/null 2>&1; then
  echo "$binary has no export-antigravity-plugin command; set CONTEXTSTREAM_MCP_BIN to a build that has it." >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

gh repo clone "$target" "$work/repo" -- --quiet
find "$work/repo" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
"$binary" export-antigravity-plugin --out "$work/repo" >/dev/null
mkdir -p "$work/repo/assets"
cp "$repo_root/assets/contextstream-logo.png" "$work/repo/assets/logo.png"
cp "$repo_root/LICENSE" "$work/repo/LICENSE"

if command -v agy >/dev/null 2>&1; then
  agy plugin validate --strict "$work/repo"
else
  echo "agy not found: skipping 'agy plugin validate --strict'." >&2
fi

cd "$work/repo"
git add -A
if git diff --cached --quiet; then
  echo "$target is up to date."
  exit 0
fi
git diff --cached --stat
if [ "$dry_run" = 1 ]; then
  echo "--dry-run: nothing pushed."
  exit 0
fi

source_commit="$(git -C "$repo_root" rev-parse --short HEAD)"
message="$(mktemp)"
{
  echo "ContextStream plugin for Antigravity, generated from mcp-server $source_commit"
  echo
  echo "By contextstream-mcp export-antigravity-plugin; see"
  echo "scripts/release/publish_antigravity_plugin.sh in contextstream/mcp-server."
  if [ -n "${PLUGIN_COMMIT_TRAILER:-}" ]; then
    echo
    echo "$PLUGIN_COMMIT_TRAILER"
  fi
} >"$message"
git commit --quiet --signoff --file "$message"
git push --quiet origin HEAD:main
echo "Published $target at $(git rev-parse --short HEAD)."
