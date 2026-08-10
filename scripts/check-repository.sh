#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "${repo_root}"

python3 -m unittest discover -s tests -v
ruff check scripts tests
ruff format --check scripts tests
shellcheck scripts/*.sh

python3 - <<'PY'
import json
from pathlib import Path

lock = dict(
    line.split("=", 1)
    for line in Path("upstream.lock").read_text(encoding="utf-8").splitlines()
    if line and not line.startswith("#")
)
compatibility = json.loads(Path("compatibility.json").read_text(encoding="utf-8"))
assert compatibility["schema"] == 1
assert compatibility["config_schema"] == 1
assert compatibility["niri_revision"] == lock["revision"]
PY

while IFS= read -r file; do
    if ! head -n 3 "${file}" | grep -q 'SPDX-License-Identifier: GPL-3.0-or-later'; then
        printf 'missing SPDX identifier: %s\n' "${file}" >&2
        exit 1
    fi
done < <(find extension scripts tests -type f \( -name '*.rs' -o -name '*.py' -o -name '*.sh' \) -print | sort)

git diff --check
