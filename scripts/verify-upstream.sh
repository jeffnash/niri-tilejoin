#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "${script_dir}/.." && pwd)
repository=$(awk -F= '$1 == "repository" { print $2 }' "${repo_root}/upstream.lock")
revision=$(awk -F= '$1 == "revision" { print $2 }' "${repo_root}/upstream.lock")
temporary=$(mktemp -d)
trap 'rm -rf -- "${temporary}"' EXIT
source_dir=${temporary}/niri

git clone --filter=blob:none --no-checkout "${repository}" "${source_dir}"
git -C "${source_dir}" checkout --detach "${revision}"
"${repo_root}/scripts/apply-patches.sh" --in-place "${source_dir}"

git -C "${source_dir}" diff --stat "${revision}...HEAD"
git -C "${source_dir}" diff --numstat "${revision}...HEAD" \
    | awk '{ added += $1; deleted += $2; files += 1 } END { printf "files=%d added=%d deleted=%d\n", files, added, deleted }'

cargo check --manifest-path "${source_dir}/Cargo.toml" --all-targets --locked
cargo clippy --manifest-path "${source_dir}/Cargo.toml" --all-targets --locked -- -D warnings
cargo test --manifest-path "${source_dir}/Cargo.toml" --lib --locked
cargo test --manifest-path "${source_dir}/Cargo.toml" -p niri-config --locked
cargo test --manifest-path "${source_dir}/Cargo.toml" -p niri-tiled --locked

if command -v nix >/dev/null 2>&1; then
    nix flake metadata --no-update-lock-file "${repo_root}" >/dev/null
    nix eval --raw "${repo_root}#packages.x86_64-linux.niri-tilejoin.drvPath" >/dev/null
    nix eval --raw "${repo_root}#packages.aarch64-linux.niri-tilejoin.drvPath" >/dev/null
fi

echo 'software update gate passed; complete docs/hardware-validation.md before changing the pin'
