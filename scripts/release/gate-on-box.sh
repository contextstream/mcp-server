#!/usr/bin/env bash
# One part of the release gate (scripts/release/gate.sh rust|checks) on the self-hosted release box, in a container of
# scripts/release/gate.Dockerfile, for .depot/workflows/release.yml when the box runs the release: no release waits for a
# rented machine. The image is built on the box's own BuildKit and kept by its tag (a hash of the Dockerfile and the Rust
# channel). The part runs on a copy of this checkout as uid 1001, in the box's relgate.slice (no CPU quota; the box's
# default seccomp profile), with cargo's registry in the volume mcp-gate-cargo and the part's own target dir (./target, as
# gate.sh expects) in mcp-gate-target-<part>, so later releases build warm.
#
#   scripts/release/gate-on-box.sh rust|checks      exit: gate.sh's
#   scripts/release/gate-on-box.sh --image          only make sure the image exists; print its tag (build-on-box.sh)
set -euo pipefail
part=${1:?rust, checks or --image}
case "$part" in rust|checks|--image) ;; *) echo "gate-on-box: unknown part $part" >&2; exit 2 ;; esac
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
say() { echo "gate-on-box: $*" >&2; }
channel=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml | head -1)
[[ "$channel" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { say "rust-toolchain.toml names no channel"; exit 2; }
tag="mcp-gate:$( (cat scripts/release/gate.Dockerfile; echo "$channel") | sha256sum | cut -c1-16)"
started=$(date +%s)
# One image build at a time on the box: the release's gate parts and its six builds all start together, and a second
# build of the same tag fails with 'image "…" already exists' (mcp-server's dry run 4241). The lock is held while
# building; whoever gets it next finds the image kept.
exec 9>/tmp/mcp-gate-image.lock
flock 9
if docker image inspect "$tag" >/dev/null 2>&1; then
  say "image $tag (kept)"
else
  say "building $tag on the box (Rust $channel)"
  context=$(mktemp -d)
  docker buildx build --builder default --load --progress=plain -t "$tag" --build-arg "RUST_CHANNEL=$channel" \
    -f scripts/release/gate.Dockerfile "$context" 2>&1 | tail -n 20 >&2
  rm -rf "$context"
  say "image built in $(( $(date +%s) - started )) s"
fi
flock -u 9
if [ "$part" = --image ]; then echo "$tag"; exit 0; fi
work=$(mktemp -d "${RUNNER_TEMP:-/tmp}/mcp-gate.XXXXXX")
cleanup() {
  docker run --rm --user 0 -v "$work:/w" "$tag" chown -R "$(id -u):$(id -g)" /w >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
cp -a "$root/." "$work/src"
docker volume create mcp-gate-cargo >/dev/null
docker volume create "mcp-gate-target-$part" >/dev/null
say "running gate.sh $part on the box (commit $(git rev-parse --short=12 HEAD), ${GATE_BOX_CGROUP_PARENT:-relgate.slice})"
code=0
docker run --rm --init --name "mcp-gate-$part-${CSRUN_RUN_NUMBER:-$$}" \
  --cgroup-parent "${GATE_BOX_CGROUP_PARENT:-relgate.slice}" --memory "${GATE_BOX_MEMORY:-48g}" --pids-limit 8192 \
  -v mcp-gate-cargo:/home/runner/.cargo -v "$work/src:/src" -v "mcp-gate-target-$part:/src/target" -w /src \
  -e CARGO_HOME=/home/runner/.cargo -e CI=true \
  "$tag" bash -c 'sudo chown runner:runner /home/runner/.cargo /src/target && sudo find /src -path /src/target -prune -o -exec chown runner:runner {} + \
    && git config --global --add safe.directory "*" && exec bash scripts/release/gate.sh "$0"' "$part" || code=$?
say "gate.sh $part ended $code after $(( $(date +%s) - started )) s"
exit "$code"
