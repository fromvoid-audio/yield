"""Owned process-group cancellation must clean nested jobs without touching peers."""

from __future__ import annotations

import fcntl
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import unittest


CI_DIR = pathlib.Path(__file__).resolve().parent
COORDINATOR = CI_DIR / "coordinator_exec.py"
MANAGED = CI_DIR / "managed_exec.py"


class ManagedExecCancellationTests(unittest.TestCase):
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
                    str(MANAGED),
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
                self.assertEqual(manager.wait(timeout=18), 128 + signal.SIGTERM)
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
