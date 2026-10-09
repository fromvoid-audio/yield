"""Safety checks for the managed binary compatibility probe."""

from __future__ import annotations

import importlib.util
from importlib.machinery import SourceFileLoader
import pathlib
import sys
import tempfile
import unittest
from contextlib import contextmanager
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[3]
CLI = ROOT / "scripts/yield-local-ci"
SPEC = importlib.util.spec_from_loader("yield_local_ci", SourceFileLoader("yield_local_ci", str(CLI)))
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ManagedBinaryProbeTests(unittest.TestCase):
    def test_command_output_can_include_stderr_without_changing_default(self) -> None:
        completed = MODULE.subprocess.CompletedProcess(
            args=["codesign"], returncode=0, stdout="", stderr="Signature=adhoc"
        )
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            self.assertEqual(MODULE.command_output("codesign"), "")
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            self.assertEqual(MODULE.command_output("codesign", combined_output=True), "Signature=adhoc")

    def test_missing_marker_refuses_before_executing_legacy_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            sentinel = root / "legacy-binary-was-run"
            binary = root / "legacy-yield"
            binary.write_text(f"#!/bin/sh\ntouch {sentinel}\n", encoding="utf-8")
            binary.chmod(0o700)
            with self.assertRaisesRegex(MODULE.CiError, "without the maintenance admission v1 marker"):
                MODULE.compiled_build_info(binary)
            self.assertFalse(sentinel.exists())

    def test_marker_scan_handles_a_chunk_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = pathlib.Path(temporary) / "binary"
            marker = MODULE.ADMISSION_MARKER
            binary.write_bytes(b"x" * (1024 * 1024 - 3) + marker)
            self.assertTrue(MODULE.binary_contains_marker(binary))

    def test_failed_build_retains_output_and_record_without_publishing_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            state_dir = root / "git-state"
            state_dir.mkdir()
            checkout = root / "checkout"
            checkout.mkdir()
            run_managed = MODULE.run_managed

            @contextmanager
            def coordinator():
                yield state_dir

            @contextmanager
            def worktree(_sha: str):
                yield checkout

            def failing_child(_cargo_command, *, cwd, env, pass_fds, stdout):
                return run_managed(
                    [
                        sys.executable,
                        "-c",
                        "import sys; print('fake cargo stdout'); print('fake cargo stderr', file=sys.stderr); sys.exit(17)",
                    ],
                    cwd=cwd,
                    env=env,
                    pass_fds=pass_fds,
                    stdout=stdout,
                )

            args = type("Args", (), {"sha": "a" * 40, "channel": "test"})()
            with (
                mock.patch.object(MODULE, "ROOT", root),
                mock.patch.object(MODULE, "common_coordinator", coordinator),
                mock.patch.object(MODULE, "exact_worktree", worktree),
                mock.patch.object(MODULE, "require_maintenance_protocol"),
                mock.patch.object(MODULE, "doctor", return_value={"host": "test"}),
                mock.patch.object(
                    MODULE, "cargo_env", return_value={"CARGO_TARGET_DIR": str(root / "target")}
                ),
                mock.patch.object(MODULE, "run_managed", side_effect=failing_child),
            ):
                with self.assertRaisesRegex(MODULE.CiError, "build failed with exit 17"):
                    MODULE.build(args)

            result_files = list((state_dir / "runs" / ("a" * 40) / "build-test").glob("*/result.json"))
            self.assertEqual(len(result_files), 1)
            record = __import__("json").loads(result_files[0].read_text(encoding="utf-8"))
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["phase"], "cargo-build")
            self.assertEqual(record["commands"][0]["exit_code"], 17)
            log = pathlib.Path(record["log"]).read_text(encoding="utf-8")
            self.assertIn("fake cargo stdout", log)
            self.assertIn("fake cargo stderr", log)
            self.assertFalse((root / "artifacts/local-ci" / ("a" * 40) / "test").exists())


if __name__ == "__main__":
    unittest.main()
