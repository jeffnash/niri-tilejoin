#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "${script_dir}/.." && pwd)
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
output=${PWD}/niri-tilejoin-support-${timestamp}.tar.gz
include_config=false
include_logs=false

usage() {
    cat <<'EOF'
usage: support-bundle.sh [--output FILE] [--include-config] [--include-logs]

Collects version, output, and DRM identity evidence. Configuration and recent user-journal logs
are excluded unless explicitly requested because they may contain private names or application
details. Review the archive before sharing it.
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --output)
            [[ $# -ge 2 ]] || { echo 'error: --output needs a path' >&2; exit 2; }
            output=$2
            shift 2
            ;;
        --include-config)
            include_config=true
            shift
            ;;
        --include-logs)
            include_logs=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

temporary=$(mktemp -d)
trap 'rm -rf -- "${temporary}"' EXIT
bundle=${temporary}/niri-tilejoin-support
mkdir -p -- "${bundle}/drm"

{
    printf 'created_utc=%s\n' "${timestamp}"
    printf 'tilejoin_repository=%s\n' "$(git -C "${repo_root}" rev-parse HEAD 2>/dev/null || echo unknown)"
    sed -n '/^[a-z_][a-z_]*=/p' "${repo_root}/upstream.lock"
    printf 'kernel=%s\n' "$(uname -srmo)"
    printf 'session_type=%s\n' "${XDG_SESSION_TYPE:-unknown}"
    printf 'desktop=%s\n' "${XDG_CURRENT_DESKTOP:-unknown}"
} >"${bundle}/manifest.txt"

if command -v niri >/dev/null 2>&1; then
    niri --version >"${bundle}/niri-version.txt" 2>&1 || true
    niri msg -j outputs >"${bundle}/outputs.json" 2>"${bundle}/outputs-error.txt" || true
fi
if command -v niri-tilejoin >/dev/null 2>&1; then
    niri-tilejoin --version >"${bundle}/niri-tilejoin-version.txt" 2>&1 || true
fi

for connector in /sys/class/drm/card*-*; do
    [[ -d ${connector} ]] || continue
    name=$(basename -- "${connector}")
    {
        printf 'connector=%s\n' "${name}"
        [[ -f ${connector}/status ]] && printf 'status=%s\n' "$(<"${connector}/status")"
        if [[ -f ${connector}/edid ]]; then
            printf 'edid_sha256=%s\n' "$(sha256sum "${connector}/edid" | awk '{print $1}')"
        fi
        if [[ -f ${connector}/modes ]]; then
            echo 'modes:'
            sed 's/^/  /' "${connector}/modes"
        fi
    } >"${bundle}/drm/${name}.txt"
done

config_home=${XDG_CONFIG_HOME:-${HOME}/.config}
if ${include_config}; then
    mkdir -p -- "${bundle}/config"
    for config in "${config_home}/niri/config.kdl" "${config_home}/niri/tilejoin.kdl"; do
        [[ -f ${config} ]] && cp -- "${config}" "${bundle}/config/"
    done
fi
if ${include_logs} && command -v journalctl >/dev/null 2>&1; then
    journalctl --user -u niri -n 1000 --no-pager >"${bundle}/niri-journal.txt" 2>&1 || true
fi

cat >"${bundle}/PRIVACY-REVIEW.txt" <<'EOF'
Review every file before sharing this archive. outputs.json and connector metadata contain hardware
identities. Files under config/ can contain application rules and user-chosen names. Journal logs
can contain application titles and paths. The helper does not collect environment variables, raw
EDID blobs, home-directory listings, or configuration/logs unless their opt-in flag was supplied.
EOF

mkdir -p -- "$(dirname -- "${output}")"
tar -C "${temporary}" -czf "${output}" niri-tilejoin-support
printf 'support bundle written to %s\n' "${output}"
printf 'review PRIVACY-REVIEW.txt and the archive contents before sharing\n'
