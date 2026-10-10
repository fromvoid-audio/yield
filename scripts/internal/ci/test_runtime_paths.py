"""Regression checks for canonical temporary paths and direct shell startup."""

from __future__ import annotations

import os
import pathlib
import platform
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from runtime_paths import canonicalize_tmpdir  # noqa: E402


ROOT = pathlib.Path(__file__).resolve().parents[3]


class RuntimePathTests(unittest.TestCase):
    def test_symlinked_tmpdir_is_exported_as_its_physical_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            real = root / "private-tmp"
            real.mkdir()
            alias = root / "tmp-link"
            alias.symlink_to(real, target_is_directory=True)
            with mock.patch.dict(os.environ, {"TMPDIR": str(alias)}):
                self.assertEqual(canonicalize_tmpdir(), real.resolve())
                self.assertEqual(os.environ["TMPDIR"], str(real.resolve()))

    def test_missing_and_non_directory_tmpdir_are_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for path in (root / "missing", root / "file"):
                if path.name == "file":
                    path.write_text("not a directory", encoding="utf-8")
                with self.subTest(path=path), mock.patch.dict(os.environ, {"TMPDIR": str(path)}):
                    with self.assertRaisesRegex(RuntimeError, "TMPDIR must resolve"):
                        canonicalize_tmpdir()

    def test_direct_cli_canonicalizes_tmpdir_before_parsing_operations(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            physical_tmp = root / "physical-tmp"
            physical_tmp.mkdir()
            alias = root / "tmp-link"
            alias.symlink_to(physical_tmp, target_is_directory=True)
            env = os.environ.copy()
            env["TMPDIR"] = str(alias)
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/yield-local-ci"), "unsupported-operation"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("invalid choice", result.stderr)
            self.assertNotIn("TMPDIR must resolve", result.stderr)

    @unittest.skipUnless(platform.system() == "Darwin", "GPUI shell entrypoint is macOS-only")
    def test_direct_shell_entry_resolves_python_before_coordinator_exec(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            common_dir = root / "git-common"
            fake_git = fake_bin / "git"
            fake_git.write_text("#!/bin/sh\nprintf '%s\\n' \"$YIELD_TEST_COMMON_DIR\"\n", encoding="utf-8")
            fake_git.chmod(0o700)
            physical_tmp = root / "tmp"
            physical_tmp.mkdir()
            tmp_alias = root / "tmp-link"
            tmp_alias.symlink_to(physical_tmp, target_is_directory=True)
            env = os.environ.copy()
            env.pop("YIELD_CI_PYTHON", None)
            env.pop("YIELD_LOCAL_CI_COORDINATOR_FD", None)
            env["YIELD_TEST_COMMON_DIR"] = str(common_dir)
            env["TMPDIR"] = str(tmp_alias)
            env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/gpui-local-ci.sh"), "unsupported-scope"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
            self.assertIn("Usage: scripts/gpui-local-ci.sh", result.stdout + result.stderr)
            self.assertNotIn("YIELD_CI_PYTHON: unbound variable", result.stderr)
            self.assertTrue((common_dir / "local-ci" / "active.lock").is_file())


if __name__ == "__main__":
    unittest.main()
