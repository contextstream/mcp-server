#!/usr/bin/env bash
# A gate part or a release binary on Latitude's BuildKit (lat-chi), for .depot/workflows/release.yml when the self-hosted box
# runs the release: the heavy compile leaves the box, whose CPU the contextstream release's validation needs. It builds the
# stage gate or binary of scripts/release/gate.Dockerfile with this checkout as the context, through a remote buildx builder
# made in a private buildx config (the release box's ssh config names relbuild-lat1 and its key). Cargo's home and each part's
# or binary's target dir are BuildKit cache mounts there, so later releases build warm. When the builder cannot be reached,
# the box runs it (gate-on-box.sh, build-on-box.sh) and the log says so; RELEASE_LAT=0 goes to the box directly.
#
#   scripts/release/on-lat.sh gate rust|checks               exit: gate.sh's
#   scripts/release/on-lat.sh build <artifact> <version>     writes out/<artifact>; exit: build.sh's
set -euo pipefail
mode=${1:?gate or build}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
say() { echo "on-lat: $*" >&2; }
case "$mode" in
  gate)
    part=${2:?rust or checks}
    case "$part" in rust|checks) ;; *) say "unknown part $part"; exit 2 ;; esac
    box=(bash scripts/release/gate-on-box.sh "$part")
    what="gate.sh $part" ;;
  build)
    artifact=${2:?artifact}; version=${3:?version}
    case "$artifact" in contextstream-mcp-*) ;; *) say "unknown artifact $artifact"; exit 2 ;; esac
    box=(bash scripts/release/build-on-box.sh "$artifact" "$version")
    what="$artifact $version" ;;
  *) say "unknown mode $mode"; exit 2 ;;
esac
on_box() { say "$1: $what runs on the box"; unset BUILDX_CONFIG; exec "${box[@]}"; }
[ "${RELEASE_LAT:-1}" != 0 ] || on_box "RELEASE_LAT=0"
channel=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml | head -1)
[[ "$channel" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { say "rust-toolchain.toml names no channel"; exit 2; }
endpoint=${RELEASE_LAT_ENDPOINT:-ssh://buildkit@relbuild-lat1}
config=$(mktemp -d "${RUNNER_TEMP:-/tmp}/mcp-lat.XXXXXX")
trap 'rm -rf "$config"' EXIT
export BUILDX_CONFIG=$config
started=$(date +%s)
timeout 60 docker buildx create --name lat --driver remote "$endpoint" >/dev/null 2>&1 \
  && timeout 90 docker buildx inspect --bootstrap lat >/dev/null 2>&1 \
  || { rm -rf "$config"; trap - EXIT; on_box "Latitude's builder is unreachable"; }
say "$what on Latitude's BuildKit (commit $(git rev-parse --short=12 HEAD), builder ready in $(( $(date +%s) - started )) s)"
args=(--builder lat --progress=plain --build-arg "RUST_CHANNEL=$channel" -f scripts/release/gate.Dockerfile)
code=0
if [ "$mode" = gate ]; then
  docker buildx build "${args[@]}" --target gate --build-arg "PART=$part" \
    --build-arg "RUN_ID=${CSRUN_RUN_NUMBER:-local}-$part-$(date +%s)-$$" --output type=cacheonly . || code=$?
else
  out=$(mktemp -d "${RUNNER_TEMP:-/tmp}/mcp-lat-out.XXXXXX")
  docker buildx build "${args[@]}" --target binary --build-arg "ARTIFACT=$artifact" --build-arg "VERSION=$version" \
    ${SOURCE_DATE_EPOCH:+--build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"} --output "type=local,dest=$out" . || code=$?
  if [ "$code" -eq 0 ]; then
    if [ -s "$out/$artifact" ]; then mkdir -p out && install -m 0755 "$out/$artifact" "out/$artifact"
    else say "the build exported no $artifact"; code=1; fi
  fi
  rm -rf "$out"
fi
say "$what ended $code after $(( $(date +%s) - started )) s on Latitude"
exit "$code"
