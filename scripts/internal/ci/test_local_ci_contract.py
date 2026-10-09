"""Safety checks for the managed binary compatibility probe."""

from __future__ import annotations

import importlib.util
from importlib.machinery import SourceFileLoader
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
CLI = ROOT / "scripts/yield-local-ci"
SPEC = importlib.util.spec_from_loader("yield_local_ci", SourceFileLoader("yield_local_ci", str(CLI)))
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ManagedBinaryProbeTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
