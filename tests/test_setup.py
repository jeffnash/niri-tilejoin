# SPDX-License-Identifier: GPL-3.0-or-later
from __future__ import annotations

import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from setup import (  # noqa: E402
    INCLUDE_LINE,
    SetupError,
    applied_patch_ids,
    bundle_digest,
    install_config,
    prepare_source,
    validate_prepared_source,
    with_tilejoin_include,
    with_validation_include,
    without_tilejoin_include,
    write_marker,
)

GROUP = """output "wall" {
    display-group {
        member "A" { position x=0 y=0 }
        member "B" { position x=1920 y=0 }
        primary "A"
    }
}"""


class SetupTests(unittest.TestCase):
    def test_bundle_digest_is_stable(self) -> None:
        self.assertEqual(bundle_digest(), bundle_digest())

    def test_include_edit_is_idempotent(self) -> None:
        initial = "input {\n}\n"
        updated = with_tilejoin_include(initial)
        self.assertEqual(with_tilejoin_include(updated), updated)
        self.assertEqual(updated.count(INCLUDE_LINE), 1)
        self.assertNotIn(INCLUDE_LINE, without_tilejoin_include(updated))

    def test_duplicate_includes_are_normalized_at_the_first_location(self) -> None:
        initial = 'input {}\ninclude "tilejoin.kdl"\nlayout {}\ninclude "tilejoin.kdl"\n'
        updated = with_tilejoin_include(initial)
        self.assertEqual(updated.count(INCLUDE_LINE), 1)
        self.assertEqual(updated.splitlines()[1], INCLUDE_LINE)

    def test_validation_include_preserves_include_location(self) -> None:
        initial = 'include "before.kdl"\n  include "tilejoin.kdl" // existing\ninput {}\n'
        updated = with_validation_include(initial, Path("/tmp/candidate.kdl"))
        self.assertEqual(
            updated.splitlines(),
            ['include "before.kdl"', '  include "/tmp/candidate.kdl"', "input {}"],
        )

    def test_config_install_validates_backs_up_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config_dir = root / "config"
            config_dir.mkdir()
            (config_dir / "config.kdl").write_text("input {\n}\n", encoding="utf-8")
            validator = self.make_validator(root, succeeds=True)

            changed, backup = install_config(config_dir, validator, GROUP)

            self.assertTrue(changed)
            self.assertIsNotNone(backup)
            assert backup is not None
            self.assertEqual((backup / "config.kdl").read_text(), "input {\n}\n")
            self.assertEqual((config_dir / "tilejoin.kdl").read_text(), GROUP + "\n")
            self.assertIn(INCLUDE_LINE, (config_dir / "config.kdl").read_text())
            self.assertEqual(install_config(config_dir, validator, GROUP), (False, None))

    def test_failed_validation_does_not_touch_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config_dir = root / "config"
            config_dir.mkdir()
            main = config_dir / "config.kdl"
            main.write_text("input {\n}\n", encoding="utf-8")
            validator = self.make_validator(root, succeeds=False)

            with self.assertRaisesRegex(RuntimeError, "command failed"):
                install_config(config_dir, validator, GROUP)

            self.assertEqual(main.read_text(), "input {\n}\n")
            self.assertFalse((config_dir / "tilejoin.kdl").exists())

    def test_second_rename_failure_rolls_back_both_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config_dir = root / "config"
            config_dir.mkdir()
            main = config_dir / "config.kdl"
            group = config_dir / "tilejoin.kdl"
            main.write_text('include "tilejoin.kdl"\ninput {}\n', encoding="utf-8")
            group.write_text('output "old" {}\n', encoding="utf-8")
            validator = self.make_validator(root, succeeds=True)
            calls = 0

            def fail_second(source: Path, destination: Path) -> None:
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise OSError("injected second rename failure")
                source.replace(destination)

            with self.assertRaisesRegex(OSError, "injected"):
                install_config(config_dir, validator, GROUP, replace_file=fail_second)

            self.assertEqual(main.read_text(), 'include "tilejoin.kdl"\ninput {}\n')
            self.assertEqual(group.read_text(), 'output "old" {}\n')

    def test_prepared_source_requires_exact_head_tree_extension_and_cleanliness(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            source, revision = self.make_git_source(Path(temporary))
            extension = source / "niri-tiled"
            extension.mkdir()
            (extension / "Cargo.toml").write_text("[package]\nname='fixture'\n", encoding="utf-8")
            (source / "patched").write_text("adapter\n", encoding="utf-8")
            self.git(source, "add", "patched")
            self.git(source, "commit", "-m", "adapter")
            expected = applied_patch_ids(source, revision)
            with patch("setup.integration_patch_ids", return_value=expected):
                write_marker(source, revision, "bundle")
                self.assertTrue(validate_prepared_source(source, revision, "bundle"))
                (source / "patched").write_text("changed\n", encoding="utf-8")
                self.git(source, "add", "patched")
                self.git(source, "commit", "-m", "changed")
                with self.assertRaisesRegex(SetupError, "expected integration patch stack"):
                    validate_prepared_source(source, revision, "bundle")

    def test_manual_source_uses_an_isolated_worktree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            source, revision = self.make_git_source(Path(temporary))

            def fake_apply(worktree: Path, expected: str, digest: str) -> None:
                self.assertEqual(expected, revision)
                extension = worktree / "niri-tiled"
                extension.mkdir()
                (extension / "Cargo.toml").write_text(
                    "[package]\nname='fixture'\n", encoding="utf-8"
                )
                write_marker(worktree, expected, digest)

            with (
                patch("setup.apply_extension", side_effect=fake_apply),
                patch("setup.integration_patch_ids", return_value=[]),
            ):
                prepared = prepare_source(source, "unused", revision, "bundle")

            self.assertNotEqual(prepared, source)
            self.assertFalse((source / "niri-tiled").exists())
            self.assertTrue((prepared / "niri-tiled" / "Cargo.toml").is_file())

    def test_apply_script_rolls_back_a_partial_patch_stack(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bundle = root / "bundle"
            script_dir = bundle / "scripts"
            patch_dir = bundle / "integration" / "niri" / "patches"
            extension = bundle / "extension" / "niri-tiled"
            script_dir.mkdir(parents=True)
            patch_dir.mkdir(parents=True)
            extension.mkdir(parents=True)
            shutil.copy2(ROOT / "scripts" / "apply-patches.sh", script_dir)
            (extension / "Cargo.toml").write_text("[package]\nname='fixture'\n", encoding="utf-8")

            source, revision = self.make_git_source(root)
            (bundle / "upstream.lock").write_text(
                f"repository=unused\nrevision={revision}\n", encoding="utf-8"
            )
            (source / "base").write_text("patched\n", encoding="utf-8")
            self.git(source, "add", "base")
            self.git(source, "commit", "-m", "valid patch")
            valid_patch = subprocess.run(
                ["git", "format-patch", "-1", "--stdout"],
                cwd=source,
                check=True,
                capture_output=True,
                text=True,
            ).stdout
            (patch_dir / "0001-valid.patch").write_text(valid_patch, encoding="utf-8")
            (patch_dir / "0002-invalid.patch").write_text("not a mail patch\n", encoding="utf-8")
            self.git(source, "reset", "--hard", revision)

            result = subprocess.run(
                [script_dir / "apply-patches.sh", source], capture_output=True, text=True
            )
            self.assertNotEqual(result.returncode, 0)
            head = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=source,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertEqual(head, revision)
            self.assertFalse((source / "niri-tiled").exists())
            self.assertEqual(
                subprocess.run(
                    ["git", "status", "--porcelain"],
                    cwd=source,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout,
                "",
            )

    def test_apply_script_prepares_an_isolated_worktree_by_default(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bundle = root / "bundle"
            script_dir = bundle / "scripts"
            patch_dir = bundle / "integration" / "niri" / "patches"
            extension = bundle / "extension" / "niri-tiled"
            script_dir.mkdir(parents=True)
            patch_dir.mkdir(parents=True)
            extension.mkdir(parents=True)
            shutil.copy2(ROOT / "scripts" / "apply-patches.sh", script_dir)
            (extension / "Cargo.toml").write_text("[package]\nname='fixture'\n", encoding="utf-8")

            source, revision = self.make_git_source(root)
            (bundle / "upstream.lock").write_text(
                f"repository=unused\nrevision={revision}\n", encoding="utf-8"
            )
            (source / "adapter").write_text("patched\n", encoding="utf-8")
            self.git(source, "add", "adapter")
            self.git(source, "commit", "-m", "adapter")
            patch_text = subprocess.run(
                ["git", "format-patch", "-1", "--stdout"],
                cwd=source,
                check=True,
                capture_output=True,
                text=True,
            ).stdout
            (patch_dir / "0001-adapter.patch").write_text(patch_text, encoding="utf-8")
            self.git(source, "reset", "--hard", revision)
            prepared = root / "prepared"

            subprocess.run(
                [script_dir / "apply-patches.sh", "--output", prepared, source],
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertEqual(
                subprocess.run(
                    ["git", "rev-parse", "HEAD"],
                    cwd=source,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip(),
                revision,
            )
            self.assertFalse((source / "adapter").exists())
            self.assertTrue((prepared / "adapter").is_file())
            self.assertTrue((prepared / "niri-tiled" / "Cargo.toml").is_file())

    @classmethod
    def make_git_source(cls, root: Path) -> tuple[Path, str]:
        source = root / "niri"
        source.mkdir()
        cls.git(source, "init", "-q")
        cls.git(source, "config", "user.name", "fixture")
        cls.git(source, "config", "user.email", "fixture@example.invalid")
        (source / "base").write_text("base\n", encoding="utf-8")
        cls.git(source, "add", "base")
        cls.git(source, "commit", "-q", "-m", "base")
        revision = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=source,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        return source, revision

    @staticmethod
    def git(source: Path, *arguments: str) -> None:
        subprocess.run(["git", *arguments], cwd=source, check=True, capture_output=True, text=True)

    @staticmethod
    def make_validator(root: Path, *, succeeds: bool) -> Path:
        validator = root / "validator"
        validator.write_text(f"#!/bin/sh\nexit {0 if succeeds else 1}\n", encoding="utf-8")
        validator.chmod(validator.stat().st_mode | stat.S_IXUSR)
        return validator


if __name__ == "__main__":
    unittest.main()
