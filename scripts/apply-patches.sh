#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

in_place=false
output=
while [[ $# -gt 0 ]]; do
    case $1 in
        --in-place)
            in_place=true
            shift
            ;;
        --output)
            [[ $# -ge 2 ]] || { echo 'error: --output needs a path' >&2; exit 2; }
            output=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
        -*)
            printf 'error: unknown option: %s\n' "$1" >&2
            exit 2
            ;;
        *)
            break
            ;;
    esac
done

repo_dir=${1:?usage: $0 [--output PATH] /path/to/niri}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "${script_dir}/.." && pwd)
patch_dir=${repo_root}/integration/niri/patches
extension_dir=${repo_root}/extension/niri-tiled
expected_revision=$(awk -F= '$1 == "revision" { print $2 }' "${repo_root}/upstream.lock")
target_extension=${repo_dir}/niri-tiled
original_head=

if ! ${in_place}; then
    [[ -z ${output} ]] && output=${repo_dir}.tilejoin-${expected_revision:0:12}
    output=$(realpath -m -- "${output}")
    if [[ -e ${output} ]]; then
        printf 'error: isolated output already exists: %s\n' "${output}" >&2
        exit 1
    fi
    git -C "${repo_dir}" worktree add --detach "${output}" "${expected_revision}"
    if ! "${BASH_SOURCE[0]}" --in-place "${output}"; then
        git -C "${repo_dir}" worktree remove --force "${output}" >/dev/null 2>&1 || true
        exit 1
    fi
    printf 'isolated tilejoin source prepared at %s\n' "${output}"
    exit 0
fi

rollback() {
    status=$?
    if [[ ${status} -eq 0 ]]; then
        status=1
    fi
    trap - ERR INT TERM
    if [[ -n "${original_head}" ]]; then
        git -C "${repo_dir}" am --abort >/dev/null 2>&1 || true
        if [[ -d "${target_extension}" ]]; then
            rm -r -- "${target_extension}"
        fi
        git -C "${repo_dir}" reset --hard "${original_head}" >/dev/null
    fi
    exit "${status}"
}

actual_revision=$(git -C "${repo_dir}" rev-parse HEAD)
if [[ "${actual_revision}" != "${expected_revision}" ]]; then
    printf 'error: expected niri at %s, found %s\n' "${expected_revision}" "${actual_revision}" >&2
    exit 1
fi

if [[ -n "$(git -C "${repo_dir}" status --porcelain)" ]]; then
    echo 'error: niri checkout must be clean before applying tilejoin patches' >&2
    exit 1
fi

if [[ -e "${target_extension}" ]]; then
    echo "error: extension target already exists: ${target_extension}" >&2
    exit 1
fi

if [[ ! -f "${extension_dir}/Cargo.toml" ]]; then
    echo "error: bundled extension is missing: ${extension_dir}" >&2
    exit 1
fi

original_head=${actual_revision}
trap rollback ERR INT TERM

git -C "${repo_dir}" \
    -c user.name=niri-tilejoin \
    -c user.email=niri-tilejoin@users.noreply.github.com \
    am --3way "${patch_dir}"/*.patch
cp -a -- "${extension_dir}" "${target_extension}"
trap - ERR INT TERM

printf 'tilejoin extension copied to %s\n' "${target_extension}"
