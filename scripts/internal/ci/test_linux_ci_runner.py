"""Exercise Linux lane dispatch without running Cargo or touching app state."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]


class LinuxCiRunnerTests(unittest.TestCase):
    def test_cli_is_side_effect_free_and_full_lane_propagates_build_failures(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-linux-ci-") as temporary:
            root = Path(temporary)
            script = root / "gpui-linux-ci.sh"
            script.write_bytes((ROOT / "scripts/gpui-linux-ci.sh").read_bytes())
            log = root / "commands"
            for tool, body in {
                "uname": 'echo Linux',
                "git": 'echo fixture-source',
                "cargo": 'echo "$*" >> "$YIELD_TEST_COMMAND_LOG"; exit "${YIELD_TEST_CARGO_EXIT:-0}"',
                "python3": 'echo "python3 $*" >> "$YIELD_TEST_COMMAND_LOG"',
                "tini": 'test "$1" = -s && test "$2" = -- || exit 99; shift 2; exec "$@"',
            }.items():
                path = root / tool
                path.write_text("#!/bin/sh\n" + body + "\n", encoding="utf-8")
                path.chmod(0o755)
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", YIELD_TEST_COMMAND_LOG=str(log))

            def invoke(*args: str) -> subprocess.CompletedProcess[str]:
                return subprocess.run(["bash", str(script), *args], env=env, capture_output=True, text=True, timeout=5)

            for args, status in ((["--help"], 0), (["unknown"], 2), (["check", "full"], 2)):
                with self.subTest(args=args):
                    self.assertEqual(invoke(*args).returncode, status)
                    self.assertFalse(log.exists())
            full = invoke("full")
            self.assertEqual(full.returncode, 0, full.stdout + full.stderr)
            self.assertEqual(log.read_text().splitlines(), [
                "fmt --all --check",
                "check --locked -p yield --bin yield",
                "check --locked --workspace --all-targets",
                "python3 -m unittest scripts.internal.ci.test_linux_ci_runner scripts.internal.ci.test_full_ci_runner scripts.internal.ci.test_runtime_paths",
                "test --workspace --locked --no-fail-fast -- --test-threads=1",
            ])
            log.unlink()
            env["YIELD_TEST_CARGO_EXIT"] = "7"
            failed = invoke("full")
            self.assertEqual(failed.returncode, 7)
            self.assertNotIn("PASS", failed.stdout)
            self.assertEqual(log.read_text().splitlines(), ["fmt --all --check"])


if __name__ == "__main__":
    unittest.main()
