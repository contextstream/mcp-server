#!/usr/bin/env bash
# One release binary (scripts/release/build.sh <artifact> <version>) on the self-hosted release box, for
# .depot/workflows/release.yml when the box runs the release: no release waits for a rented machine. It runs in the gate's
# image (scripts/release/gate-on-box.sh --image; build.sh installs its own cross tools with sudo, as anywhere), on a copy of
# this checkout as uid 1001, in the box's relgate.slice, with cargo's registry, the artifact's ./target and cargo-xwin's
# MSVC cache in named volumes. out/<artifact> comes back to this checkout. SOURCE_DATE_EPOCH is passed through.
# linux-arm64 builds natively only (build.sh), so it does not build here.
#
#   scripts/release/build-on-box.sh <artifact> <version>      exit: build.sh's
set -euo pipefail
artifact=${1:?artifact}; version=${2:?version}
case "$artifact" in contextstream-mcp-linux-arm64) echo "build-on-box: $artifact builds on an arm64 machine only" >&2; exit 2 ;; esac
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
tag=$(bash scripts/release/gate-on-box.sh --image)
volume=$(printf '%s' "$artifact" | tr -c 'a-zA-Z0-9_.-' '-')
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/mcp-build.XXXXXX")
cleanup() {
  docker run --rm --user 0 -v "$work:/w" "$tag" chown -R "$(id -u):$(id -g)" /w >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
cp -a "$root/." "$work/src"
for v in mcp-gate-cargo "mcp-build-target-$volume" mcp-build-xwin; do docker volume create "$v" >/dev/null; done
started=$(date +%s)
echo "build-on-box: building $artifact $version on the box (commit $(git rev-parse --short=12 HEAD))"
code=0
docker run --rm --init --name "mcp-build-$volume-${CSRUN_RUN_NUMBER:-$$}" \
  --cgroup-parent "${GATE_BOX_CGROUP_PARENT:-relgate.slice}" --memory "${GATE_BOX_MEMORY:-48g}" --pids-limit 8192 \
  -v mcp-gate-cargo:/home/runner/.cargo -v "$work/src:/src" -v "mcp-build-target-$volume:/src/target" \
  -v mcp-build-xwin:/home/runner/.cache/cargo-xwin -w /src \
  -e CARGO_HOME=/home/runner/.cargo -e CI=true -e "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-}" \
  "$tag" bash -c 'sudo mkdir -p /home/runner/.cache && sudo chown runner:runner /home/runner/.cargo /home/runner/.cache /home/runner/.cache/cargo-xwin /src/target \
    && sudo find /src -path /src/target -prune -o -exec chown runner:runner {} + \
    && git config --global --add safe.directory "*" && exec bash scripts/release/build.sh "$0" "$1"' "$artifact" "$version" || code=$?
if [ "$code" -eq 0 ]; then
  mkdir -p "$root/out" && cp "$work/src/out/$artifact" "$root/out/$artifact"
fi
echo "build-on-box: $artifact ended $code after $(( $(date +%s) - started )) s"
exit "$code"
