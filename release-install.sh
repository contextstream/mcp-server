#!/usr/bin/env bash
# Install the latest published ContextStream MCP release and verify PATH/version.
set -euo pipefail

SETUP_URL="https://contextstream.io/scripts/setup.sh"
LATEST_VERSION_URL="https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/latest/version.json"
EXPECTED_PATH="/usr/local/bin/contextstream-mcp"
BINARY_NAME="contextstream-mcp"
INSTALL_BIN_DIR="/usr/local/bin"
PRIMARY_LINK_NAME="release-install.sh"
ALIAS_LINK_NAME="contextstream-release-install"
SKIP_INSTALL=false
ALLOW_PATH_MISMATCH=false
ALLOW_VERSION_MISMATCH=false
LINK_SCRIPT=true

usage() {
    cat <<'EOF'
Usage:
  release-install.sh [options]

Options:
  --skip-install          Skip installer execution; only verify current binary
  --path <dir>            Install directory for script command links (default: /usr/local/bin)
  --expected-path <path>  Expected resolved binary path (default: /usr/local/bin/contextstream-mcp)
  --setup-url <url>       Override setup script URL
  --latest-url <url>      Override latest-version manifest URL
  --allow-path-mismatch   Warn only if PATH resolves to a different binary
  --allow-version-mismatch Warn only if installed version differs from latest
  --no-link-script        Do not install/update global script command links
  -h, --help              Show this help
EOF
}

log() {
    echo "[release-install] $*"
}

warn() {
    echo "[release-install][warn] $*" >&2
}

die() {
    echo "[release-install][error] $*" >&2
    exit 1
}

extract_semver() {
    local input="${1:-}"
    echo "$input" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true
}

get_latest_version() {
    local payload
    payload="$(curl -fsSL "$LATEST_VERSION_URL")" || return 1
    echo "$payload" | sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' | head -1
}

resolve_script_path() {
    local src="$1"
    while [ -L "$src" ]; do
        local dir
        dir="$(cd -P "$(dirname "$src")" >/dev/null 2>&1 && pwd)"
        src="$(readlink "$src")"
        [[ "$src" != /* ]] && src="$dir/$src"
    done
    local final_dir
    final_dir="$(cd -P "$(dirname "$src")" >/dev/null 2>&1 && pwd)"
    echo "$final_dir/$(basename "$src")"
}

install_script_links() {
    local script_path="$1"
    local primary_link="$INSTALL_BIN_DIR/$PRIMARY_LINK_NAME"
    local alias_link="$INSTALL_BIN_DIR/$ALIAS_LINK_NAME"
    local parent_dir
    parent_dir="$(dirname "$INSTALL_BIN_DIR")"

    if [ ! -d "$INSTALL_BIN_DIR" ]; then
        if [ -w "$parent_dir" ]; then
            mkdir -p "$INSTALL_BIN_DIR"
        else
            sudo mkdir -p "$INSTALL_BIN_DIR"
        fi
    fi

    log "Installing script links to $INSTALL_BIN_DIR"
    if [ -w "$INSTALL_BIN_DIR" ]; then
        ln -sfn "$script_path" "$primary_link"
        ln -sfn "$script_path" "$alias_link"
    else
        sudo ln -sfn "$script_path" "$primary_link"
        sudo ln -sfn "$script_path" "$alias_link"
    fi
    log "Script link: $primary_link"
    log "Alias link:  $alias_link"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-install)
            SKIP_INSTALL=true
            shift
            ;;
        --path)
            INSTALL_BIN_DIR="${2:-}"
            [ -n "$INSTALL_BIN_DIR" ] || die "--path requires a value"
            shift 2
            ;;
        --expected-path)
            EXPECTED_PATH="${2:-}"
            [ -n "$EXPECTED_PATH" ] || die "--expected-path requires a value"
            shift 2
            ;;
        --setup-url)
            SETUP_URL="${2:-}"
            [ -n "$SETUP_URL" ] || die "--setup-url requires a value"
            shift 2
            ;;
        --latest-url)
            LATEST_VERSION_URL="${2:-}"
            [ -n "$LATEST_VERSION_URL" ] || die "--latest-url requires a value"
            shift 2
            ;;
        --allow-path-mismatch)
            ALLOW_PATH_MISMATCH=true
            shift
            ;;
        --allow-version-mismatch)
            ALLOW_VERSION_MISMATCH=true
            shift
            ;;
        --no-link-script)
            LINK_SCRIPT=false
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "Unknown option: $1"
            ;;
    esac
done

SCRIPT_PATH="$(resolve_script_path "${BASH_SOURCE[0]}")"
[ -f "$SCRIPT_PATH" ] || die "Could not resolve script path from ${BASH_SOURCE[0]}"

if [ "$LINK_SCRIPT" = true ]; then
    install_script_links "$SCRIPT_PATH"
fi

if [ "$SKIP_INSTALL" = false ]; then
    log "Running release installer from $SETUP_URL"
    curl -fsSL "$SETUP_URL" | bash
fi

# Refresh shell command cache so command -v sees the newly installed binary.
hash -r 2>/dev/null || true

resolved_path="$(command -v "$BINARY_NAME" || true)"
[ -n "$resolved_path" ] || die "$BINARY_NAME not found on PATH after install"

version_output="$("$BINARY_NAME" --version 2>&1 || true)"
resolved_version="$(extract_semver "$version_output")"

latest_version=""
if latest_version="$(get_latest_version 2>/dev/null)"; then
    :
else
    warn "Could not fetch latest version manifest from $LATEST_VERSION_URL"
fi

all_paths="$(type -a "$BINARY_NAME" 2>/dev/null || true)"

echo ""
log "Resolved path: $resolved_path"
log "Resolved version: ${resolved_version:-unknown}"
if [ -n "$latest_version" ]; then
    log "Latest published version: $latest_version"
fi
if [ -n "$all_paths" ]; then
    log "PATH candidates:"
    echo "$all_paths"
fi
echo ""

failures=0

if [ "$resolved_path" != "$EXPECTED_PATH" ]; then
    if [ "$ALLOW_PATH_MISMATCH" = true ]; then
        warn "Resolved path does not match expected path ($EXPECTED_PATH)"
    else
        warn "Resolved path does not match expected path ($EXPECTED_PATH)"
        failures=$((failures + 1))
    fi
fi

if [ -n "$latest_version" ] && [ -n "$resolved_version" ] && [ "$resolved_version" != "$latest_version" ]; then
    if [ "$ALLOW_VERSION_MISMATCH" = true ]; then
        warn "Installed version (${resolved_version}) differs from latest (${latest_version})"
    else
        warn "Installed version (${resolved_version}) differs from latest (${latest_version})"
        failures=$((failures + 1))
    fi
fi

if [ "$failures" -gt 0 ]; then
    die "Verification failed with $failures issue(s)"
fi

log "OK: release install and verification complete."
