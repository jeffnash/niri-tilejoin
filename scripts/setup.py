#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build, install, and configure the niri-tilejoin extension."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from collections.abc import Callable, Iterable
from datetime import UTC, datetime
from pathlib import Path

from tilejoin_config import (
    ConfigOptions,
    Output,
    candidate_diagnostics,
    config_for,
    format_mode,
    load_outputs,
    select_outputs,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
INCLUDE_LINE = 'include "tilejoin.kdl"'
INCLUDE_RE = re.compile(r'^\s*include\s+"tilejoin\.kdl"\s*(?://.*)?$')


class SetupError(RuntimeError):
    """An expected setup failure with a user-facing message."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "detect a display group, build the pinned niri-tilejoin extension, and optionally "
            "install its configuration"
        )
    )
    parser.add_argument("connectors", nargs="*", help="two to four connector names")
    parser.add_argument("--name", default="display-wall", help="logical output name")
    parser.add_argument("--scale", type=float, help="logical output scale")
    parser.add_argument(
        "--relaxed-refresh",
        action="store_true",
        help="permit up to 100 mHz of member refresh mismatch",
    )
    parser.add_argument(
        "--composited",
        action="store_true",
        help="disable per-member direct scanout",
    )
    parser.add_argument(
        "--json-file",
        type=Path,
        help="read niri output JSON from a file instead of the running compositor",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list detected outputs and exit without building",
    )
    parser.add_argument(
        "--diagnose",
        action="store_true",
        help="explain why every two-to-four-output candidate is accepted or rejected",
    )
    parser.add_argument(
        "--source",
        type=Path,
        help="use this niri checkout instead of the content-addressed build cache",
    )
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "niri-tilejoin",
        help="build-cache parent (default: %(default)s)",
    )
    parser.add_argument(
        "--prefix",
        type=Path,
        default=Path.home() / ".local",
        help="installation prefix (default: %(default)s)",
    )
    parser.add_argument(
        "--config-dir",
        type=Path,
        default=Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "niri",
        help="niri configuration directory (default: %(default)s)",
    )
    parser.add_argument(
        "--write-config",
        action="store_true",
        help="validate, back up, and install tilejoin.kdl plus its include",
    )
    parser.add_argument(
        "--no-install",
        action="store_true",
        help="build but do not copy the binary into PREFIX/bin",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="reuse SOURCE/target/release/niri (or --validator) without compiling",
    )
    parser.add_argument(
        "--validator",
        type=Path,
        help="patched niri binary to use for validation when --skip-build is set",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="show the detected configuration and planned paths without changing files",
    )
    return parser.parse_args()


def read_lock(path: Path = REPO_ROOT / "upstream.lock") -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.lstrip().startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator or not key or not value:
            raise SetupError(f"invalid lock-file line: {line!r}")
        values[key] = value
    if "repository" not in values or "revision" not in values:
        raise SetupError("upstream.lock must define repository and revision")
    return values


def bundle_digest(root: Path = REPO_ROOT) -> str:
    digest = hashlib.sha256()
    roots = [
        root / "extension" / "niri-tiled",
        root / "integration" / "niri" / "patches",
    ]
    files = [root / "scripts" / "apply-patches.sh", root / "upstream.lock"]
    for directory in roots:
        files.extend(path for path in directory.rglob("*") if path.is_file())
    for path in sorted(files):
        digest.update(str(path.relative_to(root)).encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def run(
    command: Iterable[os.PathLike[str] | str],
    *,
    cwd: Path | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    rendered = [os.fspath(part) for part in command]
    try:
        return subprocess.run(
            rendered,
            cwd=cwd,
            check=True,
            text=True,
            capture_output=capture,
        )
    except FileNotFoundError as error:
        raise SetupError(f"required command is not installed: {rendered[0]}") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        suffix = f": {detail}" if detail else ""
        raise SetupError(f"command failed ({' '.join(rendered)}){suffix}") from error


def default_source(cache_dir: Path, revision: str, digest: str) -> Path:
    return cache_dir.expanduser() / f"niri-{revision[:12]}-{digest[:12]}"


def marker_path(source: Path) -> Path:
    return source / ".niri-tilejoin-build.json"


def extension_digest(source: Path) -> str:
    digest = hashlib.sha256()
    root = source / "niri-tiled"
    if not root.is_dir():
        raise SetupError(f"prepared source is missing the injected extension: {source}")
    for path in sorted(path for path in root.rglob("*") if path.is_file()):
        digest.update(str(path.relative_to(root)).encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def patch_ids(patch_stream: str) -> list[str]:
    try:
        result = subprocess.run(
            ["git", "patch-id", "--stable"],
            input=patch_stream,
            check=True,
            capture_output=True,
            text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as error:
        raise SetupError(f"could not compute integration patch identity: {error}") from error
    return [line.split()[0] for line in result.stdout.splitlines() if line]


def integration_patch_ids() -> list[str]:
    patches = sorted((REPO_ROOT / "integration" / "niri" / "patches").glob("*.patch"))
    return patch_ids("".join(path.read_text(encoding="utf-8") for path in patches))


def applied_patch_ids(source: Path, revision: str) -> list[str]:
    stream = run(
        ["git", "format-patch", "--stdout", f"{revision}..HEAD"],
        cwd=source,
        capture=True,
    ).stdout
    return patch_ids(stream)


def source_identity(source: Path, revision: str, digest: str) -> dict[str, object]:
    expected_patches = integration_patch_ids()
    actual_patches = applied_patch_ids(source, revision)
    if actual_patches != expected_patches:
        raise SetupError(
            "prepared source does not contain exactly the expected integration patch stack: "
            f"{source}"
        )
    return {
        "revision": revision,
        "bundle_digest": digest,
        "patched_head": run(["git", "rev-parse", "HEAD"], cwd=source, capture=True).stdout.strip(),
        "patched_tree": run(
            ["git", "rev-parse", "HEAD^{tree}"], cwd=source, capture=True
        ).stdout.strip(),
        "extension_digest": extension_digest(source),
        "patch_ids": actual_patches,
    }


def write_marker(source: Path, revision: str, digest: str) -> None:
    marker_path(source).write_text(
        json.dumps(source_identity(source, revision, digest), indent=2) + "\n",
        encoding="utf-8",
    )


def validate_prepared_source(source: Path, revision: str, digest: str) -> bool:
    marker = marker_path(source)
    if not marker.is_file():
        return False
    try:
        value = json.loads(marker.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as error:
        raise SetupError(f"invalid build marker in {source}: {error}") from error
    if value.get("revision") != revision or value.get("bundle_digest") != digest:
        raise SetupError(
            f"{source} belongs to a different tilejoin bundle; omit --source to use a new "
            "content-addressed build directory"
        )
    expected = source_identity(source, revision, digest)
    if value != expected:
        raise SetupError(
            f"prepared source has changed since integration: {source}; omit --source to use a "
            "new content-addressed build directory"
        )
    status = run(
        ["git", "status", "--porcelain", "--untracked-files=all"],
        cwd=source,
        capture=True,
    ).stdout.splitlines()
    unexpected = [
        line
        for line in status
        if not line.endswith(" .niri-tilejoin-build.json") and " niri-tiled/" not in line
    ]
    if unexpected:
        raise SetupError(f"prepared source is not clean: {source}: {unexpected[0]}")
    run(
        ["git", "merge-base", "--is-ancestor", revision, value["patched_head"]],
        cwd=source,
    )
    return True


def apply_extension(source: Path, revision: str, digest: str) -> None:
    current = run(["git", "rev-parse", "HEAD"], cwd=source, capture=True).stdout.strip()
    if current != revision:
        raise SetupError(f"expected clean niri revision {revision}, found {current}")
    status = run(["git", "status", "--porcelain"], cwd=source, capture=True).stdout
    if status:
        raise SetupError(f"niri checkout must be clean before integration: {source}")
    run([REPO_ROOT / "scripts" / "apply-patches.sh", "--in-place", source])
    write_marker(source, revision, digest)


def clone_and_prepare(source: Path, repository: str, revision: str, digest: str) -> None:
    source.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=f".{source.name}.", dir=source.parent))
    try:
        run(
            [
                "git",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                repository,
                temporary,
            ]
        )
        run(["git", "checkout", "--detach", revision], cwd=temporary)
        apply_extension(temporary, revision, digest)
        os.replace(temporary, source)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise


def prepare_source(source: Path, repository: str, revision: str, digest: str) -> Path:
    source = source.expanduser().resolve()
    if source.exists():
        if validate_prepared_source(source, revision, digest):
            return source
        isolated = source.parent / f".{source.name}.tilejoin-{revision[:12]}-{digest[:12]}"
        if isolated.exists():
            if validate_prepared_source(isolated, revision, digest):
                return isolated
            raise SetupError(f"isolated tilejoin worktree has unexpected contents: {isolated}")
        current = run(["git", "rev-parse", "HEAD"], cwd=source, capture=True).stdout.strip()
        if current != revision:
            raise SetupError(f"expected niri revision {revision}, found {current}")
        status = run(["git", "status", "--porcelain"], cwd=source, capture=True).stdout
        if status:
            raise SetupError(f"niri checkout must be clean before integration: {source}")
        run(["git", "worktree", "add", "--detach", isolated, revision], cwd=source)
        try:
            apply_extension(isolated, revision, digest)
        except BaseException:
            run(["git", "worktree", "remove", "--force", isolated], cwd=source)
            raise
        return isolated
    clone_and_prepare(source, repository, revision, digest)
    return source


def atomic_copy(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        dir=destination.parent,
        prefix=f".{destination.name}.",
        delete=False,
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        shutil.copy2(source, temporary_path)
        temporary_path.chmod(0o755)
        os.replace(temporary_path, destination)
        fsync_directory(destination.parent)
    finally:
        temporary_path.unlink(missing_ok=True)


def atomic_write(destination: Path, contents: str) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    mode = destination.stat().st_mode & 0o777 if destination.exists() else 0o644
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=destination.parent,
        prefix=f".{destination.name}.",
        delete=False,
    ) as temporary:
        temporary.write(contents)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    try:
        temporary_path.chmod(mode)
        os.replace(temporary_path, destination)
        fsync_directory(destination.parent)
    finally:
        temporary_path.unlink(missing_ok=True)


def with_tilejoin_include(config: str) -> str:
    lines = config.splitlines()
    matches = [index for index, line in enumerate(lines) if INCLUDE_RE.fullmatch(line)]
    if matches:
        first = matches[0]
        lines[first] = INCLUDE_LINE
        lines = [
            line
            for index, line in enumerate(lines)
            if index == first or not INCLUDE_RE.fullmatch(line)
        ]
        return "\n".join(lines) + "\n"
    if config and not config.endswith("\n"):
        config += "\n"
    return config + ("\n" if config else "") + INCLUDE_LINE + "\n"


def without_tilejoin_include(config: str) -> str:
    return "\n".join(line for line in config.splitlines() if not INCLUDE_RE.fullmatch(line))


def with_validation_include(config: str, group_path: Path) -> str:
    replacement = f"include {json.dumps(str(group_path))}"
    lines = config.splitlines()
    found = False
    updated = []
    for line in lines:
        if INCLUDE_RE.fullmatch(line):
            if found:
                continue
            indentation = line[: len(line) - len(line.lstrip())]
            updated.append(indentation + replacement)
            found = True
        else:
            updated.append(line)
    if found:
        return "\n".join(updated) + "\n"
    suffix = "\n\n" if config.rstrip() else ""
    return config.rstrip() + suffix + replacement + "\n"


def fsync_directory(directory: Path) -> None:
    descriptor = os.open(directory, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_staged_file(directory: Path, name: str, contents: str, mode: int) -> Path:
    descriptor, filename = tempfile.mkstemp(prefix=f".{name}.", dir=directory)
    path = Path(filename)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as file:
            file.write(contents)
            file.flush()
            os.fsync(file.fileno())
        path.chmod(mode)
        return path
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def validate_config(validator: Path, config_dir: Path, main_config: str, group_config: str) -> None:
    config_dir.mkdir(parents=True, exist_ok=True)
    group_fd, group_name = tempfile.mkstemp(
        prefix=".tilejoin-group.", suffix=".kdl", dir=config_dir
    )
    main_fd, main_name = tempfile.mkstemp(prefix=".tilejoin-main.", suffix=".kdl", dir=config_dir)
    group_path = Path(group_name)
    main_path = Path(main_name)
    try:
        with os.fdopen(group_fd, "w", encoding="utf-8") as group_file:
            group_file.write(group_config.rstrip() + "\n")
        candidate = with_validation_include(main_config, group_path)
        with os.fdopen(main_fd, "w", encoding="utf-8") as main_file:
            main_file.write(candidate)
        run([validator, "validate", "-c", main_path], capture=True)
    finally:
        group_path.unlink(missing_ok=True)
        main_path.unlink(missing_ok=True)


def install_config(
    config_dir: Path,
    validator: Path,
    group_config: str,
    *,
    replace_file: Callable[[Path, Path], None] = os.replace,
) -> tuple[bool, Path | None]:
    config_dir = config_dir.expanduser().resolve()
    main_path = config_dir / "config.kdl"
    group_path = config_dir / "tilejoin.kdl"
    old_main = main_path.read_text(encoding="utf-8") if main_path.exists() else ""
    old_group = group_path.read_text(encoding="utf-8") if group_path.exists() else ""
    had_main = main_path.exists()
    had_group = group_path.exists()
    new_main = with_tilejoin_include(old_main)
    new_group = group_config.rstrip() + "\n"
    validate_config(validator, config_dir, old_main, new_group)
    if old_main == new_main and old_group == new_group:
        return False, None

    backup = None
    if main_path.exists() or group_path.exists():
        timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%S%fZ")
        backup = config_dir / "tilejoin-backups" / timestamp
        backup.mkdir(parents=True)
        if main_path.exists():
            shutil.copy2(main_path, backup / main_path.name)
        if group_path.exists():
            shutil.copy2(group_path, backup / group_path.name)
        for path in backup.iterdir():
            with path.open("rb") as file:
                os.fsync(file.fileno())
        fsync_directory(backup)
        fsync_directory(backup.parent)

    main_mode = main_path.stat().st_mode & 0o777 if main_path.exists() else 0o644
    group_mode = group_path.stat().st_mode & 0o777 if group_path.exists() else 0o644
    staged_group = write_staged_file(config_dir, group_path.name, new_group, group_mode)
    staged_main = write_staged_file(config_dir, main_path.name, new_main, main_mode)
    try:
        replace_file(staged_group, group_path)
        replace_file(staged_main, main_path)
        fsync_directory(config_dir)
    except BaseException:
        staged_group.unlink(missing_ok=True)
        staged_main.unlink(missing_ok=True)
        if had_group:
            atomic_write(group_path, old_group)
        else:
            group_path.unlink(missing_ok=True)
        if had_main:
            atomic_write(main_path, old_main)
        else:
            main_path.unlink(missing_ok=True)
        fsync_directory(config_dir)
        raise
    return True, backup


def describe_outputs(outputs: list[Output]) -> None:
    for output in outputs:
        width, height = output.transformed_size
        print(
            f"{output.connector}: {format_mode(output.mode)}, transform {output.transform}, "
            f"post-transform {width}x{height}, selector {output.stable_selector!r}"
        )


def execute(args: argparse.Namespace) -> None:
    outputs = load_outputs(os.fspath(args.json_file) if args.json_file else None)
    if args.list:
        describe_outputs(outputs)
        return
    if args.diagnose:
        print("\n".join(candidate_diagnostics(outputs, args.relaxed_refresh)))
        return
    selected = select_outputs(outputs, args.connectors, args.relaxed_refresh)
    group_config = config_for(
        selected,
        ConfigOptions(
            name=args.name,
            scale=args.scale,
            relaxed_refresh=args.relaxed_refresh,
            composited=args.composited,
        ),
        outputs,
    )

    lock = read_lock()
    digest = bundle_digest()
    source = args.source or default_source(args.cache_dir, lock["revision"], digest)
    install_path = args.prefix.expanduser() / "bin" / "niri-tilejoin"

    print("Detected display-group configuration:\n")
    print(group_config)
    print(f"\nPinned niri source: {source.expanduser()}")
    if not args.no_install:
        print(f"Installed binary: {install_path}")
    if args.write_config:
        print(f"Configuration: {args.config_dir.expanduser() / 'tilejoin.kdl'}")
    if args.dry_run:
        print("\nDry run: no source, binary, or configuration files were changed.")
        return

    prepared = prepare_source(source, lock["repository"], lock["revision"], digest)
    binary = (
        args.validator.expanduser().resolve()
        if args.validator
        else prepared / "target/release/niri"
    )
    if not args.skip_build:
        run(["cargo", "build", "--release", "--locked", "-p", "niri"], cwd=prepared)
    if not binary.is_file():
        raise SetupError(f"patched niri binary not found: {binary}")
    if not args.no_install:
        atomic_copy(binary, install_path)

    if args.write_config:
        changed, backup = install_config(args.config_dir, binary, group_config)
        if backup is not None:
            print(f"Configuration installed; previous files backed up in {backup}")
        elif changed:
            print("Configuration installed; there were no previous files to back up.")
        else:
            print("Configuration already matches the generated display group.")
    else:
        print("Configuration was not changed; rerun with --write-config after reviewing it.")

    print(
        "Start a test session with the niri-tilejoin binary. This assistant does not restart "
        "your running compositor."
    )


def main() -> None:
    try:
        execute(parse_args())
    except SetupError as error:
        raise SystemExit(f"error: {error}") from None


if __name__ == "__main__":
    main()
