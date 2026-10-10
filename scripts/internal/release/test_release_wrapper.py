#!/usr/bin/env python3
"""Subprocess tests for the local release wrapper using temporary command fixtures."""
from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class ReleaseWrapperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="yield-release-wrapper-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        self.wrapper = self.scripts / "release.sh"
        source = Path(__file__).resolve().parents[2] / "release.sh"
        self.wrapper.write_bytes(source.read_bytes())
        self.wrapper.chmod(0o755)
        self.delegate = self.scripts / "yield-local-ci"
        self.argv_file = self.root / "delegate-argv.txt"
        self.delegate.write_text(
            "#!/usr/bin/env bash\n"
            "printf '%s\\0' \"$@\" > \"$YIELD_TEST_ARGV\"\n"
            "exit \"$YIELD_TEST_STATUS\"\n",
            encoding="utf-8",
        )
        self.delegate.chmod(0o755)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.external_log = self.root / "external-calls.txt"
        for name in ("git", "gh"):
            command = self.bin / name
            command.write_text(
                "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$YIELD_TEST_EXTERNAL_LOG\"\n",
                encoding="utf-8",
            )
            command.chmod(0o755)

    def run_wrapper(self, *args, status=0):
        env = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "YIELD_TEST_ARGV": str(self.argv_file),
            "YIELD_TEST_STATUS": str(status),
            "YIELD_TEST_EXTERNAL_LOG": str(self.external_log),
        }
        return subprocess.run(
            [str(self.wrapper), *args], cwd=self.root, env=env,
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5,
        )

    def test_build_delegates_arguments_verbatim_and_preserves_exit_status(self):
        args = (
            "build", "--sha", "a" * 40, "--channel", "beta",
            "--label", "local beta candidate",
        )
        result = self.run_wrapper(*args, status=37)
        self.assertEqual(result.returncode, 37, result.stderr)
        self.assertEqual(
            self.argv_file.read_bytes().split(b"\0")[:-1],
            [arg.encode() for arg in args],
        )

    def test_obsolete_commands_and_dispatch_fail_before_external_commands(self):
        cases = (
            ("prepare", "--bump", "patch", "--dispatch"),
            ("rc", "--version", "1.2.3", "--dispatch"),
            ("stable", "--dispatch"),
            ("build", "--sha", "a" * 40, "--channel", "release", "--dispatch"),
            ("--dispatch",),
        )
        for args in cases:
            with self.subTest(args=args):
                result = self.run_wrapper(*args)
                self.assertEqual(result.returncode, 2)
                self.assertIn("yield-local-ci build", result.stderr)
                self.assertFalse(self.argv_file.exists())
                self.assertFalse(self.external_log.exists())


if __name__ == "__main__":
    unittest.main()
