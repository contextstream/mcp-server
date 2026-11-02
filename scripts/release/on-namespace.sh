#!/usr/bin/env bash
# One release step's command on Namespace through scripts/dev/ns-run, for .depot/workflows/release.yml when csrun runs it
# (act on a self-hosted box, where nothing compiles; builds and the gate run on Namespace). The Namespace token comes from
# the job's REPOS_NAMESPACE_TOKEN (a token file's JSON, {"bearer_token": ...}): it is written to a private file only ns-run
# reads, removed at exit, never printed, and not passed on in the environment. A release checkout may have no origin/main,
# so the base is then the release commit itself (an empty patch). With --artifact PATH, the file the instance left at PATH
# comes back to PATH in this checkout.
#
#   scripts/release/on-namespace.sh [ns-run options] [--artifact PATH]... -- <command...>
#
# Exit: the command's (through ns-run), or 5 when the token is missing or malformed, or 4 when an artifact did not come back.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
artifacts=(); args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --artifact) artifacts+=("${2:?--artifact needs a path}"); args+=("$1" "$2"); shift 2 ;;
    --) break ;;
    *) args+=("$1"); shift ;;
  esac
done
[[ -n "${REPOS_NAMESPACE_TOKEN:-}" ]] || { echo "on-namespace: REPOS_NAMESPACE_TOKEN is not set" >&2; exit 5; }
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
chmod 700 "$dir"
(umask 077 && printf '%s' "$REPOS_NAMESPACE_TOKEN" > "$dir/token.json")
python3 - "$dir/token.json" <<'PY' || { echo "on-namespace: REPOS_NAMESPACE_TOKEN is not a Namespace token file ({\"bearer_token\": ...})" >&2; exit 5; }
import json, sys
token = json.load(open(sys.argv[1])).get("bearer_token")
sys.exit(0 if isinstance(token, str) and token.strip() else 1)
PY
git rev-parse -q --verify refs/remotes/origin/main >/dev/null || git update-ref refs/remotes/origin/main HEAD
rm -rf .depot-run-artifacts
code=0
env -u REPOS_NAMESPACE_TOKEN -u NSC_TOKEN NSC_TOKEN_FILE="$dir/token.json" scripts/dev/ns-run "${args[@]}" "$@" || code=$?
for path in "${artifacts[@]}"; do
  found=()
  for candidate in .depot-run-artifacts/*/"$path"; do [[ -f "$candidate" ]] && found+=("$candidate"); done
  if [[ ${#found[@]} -ne 1 ]]; then
    echo "on-namespace: $path did not come back from the instance" >&2
    [[ $code -ne 0 ]] || code=4
    continue
  fi
  mkdir -p "$(dirname "$path")"
  cp "${found[0]}" "$path"
done
exit "$code"
