"""Owned process-group cancellation must clean nested jobs without touching peers."""

from __future__ import annotations

import fcntl
import importlib.util
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


CI_DIR = pathlib.Path(__file__).resolve().parent
COORDINATOR = CI_DIR / "coordinator_exec.py"
MANAGED = CI_DIR / "managed_exec.py"
SPEC = importlib.util.spec_from_file_location("yield_managed_exec_test", MANAGED)
assert SPEC is not None and SPEC.loader is not None
MANAGED_MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MANAGED_MODULE)


class ManagedExecCancellationTests(unittest.TestCase):
    def test_unexpected_wait_error_still_joins_owned_group(self) -> None:
        process = mock.Mock()
        process.pid = 4242
        process.wait.side_effect = RuntimeError("simulated wait failure")
        with (
            mock.patch.object(MANAGED_MODULE.subprocess, "Popen", return_value=process),
            mock.patch.object(MANAGED_MODULE, "terminate_owned_group", return_value=True) as terminate,
            mock.patch.object(
                MANAGED_MODULE.sys,
                "argv",
                [str(MANAGED), "--timeout-seconds", "60", "fake-validator"],
            ),
        ):
            with self.assertRaisesRegex(RuntimeError, "simulated wait failure"):
                MANAGED_MODULE.main()

        terminate.assert_called_once_with(
            process.pid,
            process,
            grace_seconds=MANAGED_MODULE.OUTER_CLEANUP_GRACE_SECONDS,
        )

    def test_sigterm_escalates_to_nested_child_and_releases_lock_only_for_owned_tree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            lock = root / "active.lock"
            term_marker = root / "child-received-term"
            child = f"""import pathlib, signal, time, sys
def stop(_signum, _frame):
    pathlib.Path({str(term_marker)!r}).write_text('stopped')
    raise SystemExit(0)
signal.signal(signal.SIGINT, signal.SIG_IGN)
signal.signal(signal.SIGTERM, stop)
print('nested-ready', flush=True)
while True:
    time.sleep(1)
"""
            supervisor = f"""import os, subprocess, sys, time
fd = int(os.environ['YIELD_LOCAL_CI_COORDINATOR_FD'])
child = {child!r}
process = subprocess.Popen([sys.executable, '-c', child], pass_fds=(fd,), stdout=subprocess.PIPE, text=True)
assert process.stdout.readline().strip() == 'nested-ready'
print(f'supervisor-ready:{{os.getpgrp()}}', flush=True)
process.wait()
"""
            manager = subprocess.Popen(
                [
                    sys.executable,
                    str(COORDINATOR),
                    str(lock),
                    sys.executable,
                    "-c",
                    (
                        "import pathlib, sys\n"
                        f"sys.path.insert(0, {str(CI_DIR)!r})\n"
                        "import managed_exec\n"
                        "managed_exec.OUTER_CLEANUP_GRACE_SECONDS = (0.25, 0.25, 1.0)\n"
                        f"sys.argv = [{str(MANAGED)!r}, *sys.argv[1:]]\n"
                        "raise SystemExit(managed_exec.main())\n"
                    ),
                    "--timeout-seconds",
                    "60",
                    sys.executable,
                    "-c",
                    supervisor,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            sibling = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                start_new_session=True,
            )
            owned_pgid = None
            try:
                assert manager.stdout is not None
                ready = manager.stdout.readline().strip()
                self.assertTrue(ready.startswith("supervisor-ready:"), manager.stderr.read() if manager.poll() is not None else "")
                owned_pgid = int(ready.partition(":")[2])
                os.kill(manager.pid, signal.SIGTERM)
                manager_status = manager.wait(timeout=18)
                if manager_status != 128 + signal.SIGTERM:
                    import select

                    diagnostic = ""
                    if manager.stderr is not None and select.select([manager.stderr], [], [], 0.1)[0]:
                        os.set_blocking(manager.stderr.fileno(), False)
                        diagnostic = os.read(manager.stderr.fileno(), 4096).decode("utf-8", errors="replace")
                    self.fail(
                        f"managed cancellation returned {manager_status}: "
                        f"{diagnostic}"
                    )
                self.assertTrue(term_marker.is_file(), "nested SIGINT-resistant child was not escalated to SIGTERM")
                self.assertIsNone(sibling.poll(), "cancellation signaled an unrelated process group")

                contender = os.open(lock, os.O_RDWR)
                try:
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(contender, fcntl.LOCK_UN)
                finally:
                    os.close(contender)
            finally:
                if manager.poll() is None:
                    manager.kill()
                    manager.wait(timeout=3)
                if owned_pgid is not None:
                    try:
                        os.killpg(owned_pgid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if sibling.poll() is None:
                    os.killpg(sibling.pid, signal.SIGTERM)
                    sibling.wait(timeout=3)
                if manager.stdout is not None:
                    manager.stdout.close()
                if manager.stderr is not None:
                    manager.stderr.close()


if __name__ == "__main__":
    unittest.main()
