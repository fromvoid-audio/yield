"""Tests for safe coordinator-lock opening and descendant custody."""

from __future__ import annotations

import fcntl
import os
import pathlib
import select
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPT = pathlib.Path(__file__).with_name("coordinator_exec.py")


class CoordinatorExecTests(unittest.TestCase):
    def test_special_file_and_symlink_are_refused_without_blocking(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            fifo = root / "fifo.lock"
            os.mkfifo(fifo)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), str(fifo), sys.executable, "-c", "pass"],
                capture_output=True,
                text=True,
                timeout=3,
            )
            self.assertNotEqual(result.returncode, 0)

            actual = root / "actual.lock"
            actual.touch()
            linked = root / "linked.lock"
            linked.symlink_to(actual)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), str(linked), sys.executable, "-c", "pass"],
                capture_output=True,
                text=True,
                timeout=3,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cannot safely open", result.stderr)

    def test_descendant_keeps_shared_coordinator_lock_after_parent_is_killed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            lock_path = root / "active.lock"
            release_file = root / "release-child"
            child_source = f"""import pathlib, time
print('child-ready', flush=True)
release = pathlib.Path({str(release_file)!r})
while not release.exists():
    time.sleep(0.01)
"""
            child_program = f"""import os, subprocess, sys, time
fd = int(os.environ['YIELD_LOCAL_CI_COORDINATOR_FD'])
child = {child_source!r}
child_proc = subprocess.Popen([sys.executable, '-c', child], pass_fds=(fd,), stdout=subprocess.PIPE, text=True)
assert child_proc.stdout.readline().strip() == 'child-ready'
print('ready', flush=True)
time.sleep(30)
"""
            parent = subprocess.Popen(
                [sys.executable, str(SCRIPT), str(lock_path), sys.executable, "-c", child_program],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            assert parent.stdout is not None
            ready, _, _ = select.select([parent.stdout], [], [], 3)
            self.assertTrue(ready, f"coordinator child did not report readiness: {parent.stderr.read() if parent.poll() is not None else ''}")
            self.assertEqual(parent.stdout.readline().strip(), "ready")
            parent.kill()
            parent.wait(timeout=3)

            contender = os.open(lock_path, os.O_RDWR)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                release_file.touch()
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    try:
                        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        break
                    except BlockingIOError:
                        time.sleep(0.05)
                else:
                    self.fail("descendant did not release the coordinator lock after exiting")
            finally:
                release_file.touch()
                try:
                    fcntl.flock(contender, fcntl.LOCK_UN)
                finally:
                    os.close(contender)
                    parent.stdout.close()
                    if parent.stderr is not None:
                        parent.stderr.close()

    def test_runtime_alias_makes_literal_python3_import_tomllib(self) -> None:
        runtime_script = pathlib.Path(__file__).with_name("python_runtime.sh")
        command = (
            f"source {str(runtime_script)!r}; yield_prepare_python_runtime; "
            "python3 -c 'import tomllib,sys; print(sys.version_info.major, sys.version_info.minor)'"
        )
        completed = subprocess.run(
            ["bash", "-c", command],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        major, minor = map(int, completed.stdout.split())
        self.assertEqual(major, 3)
        self.assertGreaterEqual(minor, 11)


if __name__ == "__main__":
    unittest.main()
