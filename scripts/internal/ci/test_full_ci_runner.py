"""Cancellation ordering checks for the full-validation state transaction."""

from __future__ import annotations

import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest


class FullCiRunnerCancellationTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups and signals")
    def test_full_validation_waits_for_child_before_restoring_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-cancel-") as temporary:
            root = pathlib.Path(temporary)
            checkout = root / "checkout"
            script_dir = checkout / "scripts"
            script_dir.mkdir(parents=True)
            full_script = script_dir / "gpui-local-ci.sh"
            full_script.write_text("#!/bin/sh\nexec \"$YIELD_TEST_CHILD\"\n", encoding="utf-8")

            child_ready = root / "child-ready"
            child_exited = root / "child-exited"
            runner_handler_ready = root / "runner-handler-ready"
            restored = root / "state-restored"
            child = root / "fake-validator"
            child.write_text(
                "#!" + sys.executable + "\n"
                "import pathlib, signal, time\n"
                f"ready = pathlib.Path({str(child_ready)!r})\n"
                f"exited = pathlib.Path({str(child_exited)!r})\n"
                "def stop(_signum, _frame):\n"
                "    print('fake validator output after SIGINT', flush=True)\n"
                "    exited.write_text('child exited')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, stop)\n"
                "ready.write_text('ready')\n"
                "while True:\n"
                "    time.sleep(1)\n",
                encoding="utf-8",
            )
            child.chmod(0o755)

            wrapper = root / "invoke_full_ci.py"
            module_dir = pathlib.Path(__file__).resolve().parent
            wrapper.write_text(
                "import os, pathlib, sys\n"
                f"sys.path.insert(0, {str(module_dir)!r})\n"
                "import run_full_ci as runner\n"
                "from contextlib import contextmanager\n"
                f"child_exited = pathlib.Path({str(child_exited)!r})\n"
                f"handler_ready = pathlib.Path({str(runner_handler_ready)!r})\n"
                f"restored = pathlib.Path({str(restored)!r})\n"
                "original_run = runner.run\n"
                "original_popen = runner.subprocess.Popen\n"
                "original_catch = runner.catch_cancellation_signals\n"
                "children = []\n"
                "def tracked_popen(*args, **kwargs):\n"
                "    process = original_popen(*args, **kwargs)\n"
                "    children.append(process)\n"
                "    return process\n"
                "@contextmanager\n"
                "def tracked_catch():\n"
                "    with original_catch():\n"
                "        handler_ready.write_text('handler installed')\n"
                "        yield\n"
                "def run_or_skip_python_safety(command, env, pass_fds):\n"
                "    if '-m' in command and 'unittest' in command:\n"
                "        return 0\n"
                "    return original_run(command, env, pass_fds)\n"
                "def prepare(_namespace, artifact=None):\n"
                "    return ({'transaction': 'isolated-test-transaction'}, "
                "os.open(os.devnull, os.O_RDONLY), os.open(os.devnull, os.O_RDONLY))\n"
                "def convert(_namespace, namespace_fd, _transition_fd):\n"
                "    return namespace_fd\n"
                "def restore(_transaction):\n"
                "    assert children and children[-1].returncode is not None, "
                "'restore preceded full-validation child reaping'\n"
                "    assert child_exited.exists(), 'restore preceded fake child exit'\n"
                "    restored.write_text('restored after child exit')\n"
                "    return {'restored': True}\n"
                "runner.sys.platform = 'darwin'\n"
                "runner.run = run_or_skip_python_safety\n"
                "runner.subprocess.Popen = tracked_popen\n"
                "runner.catch_cancellation_signals = tracked_catch\n"
                "runner.NATIVE_PRODUCTION_CASES = ()\n"
                "runner.local_state.prepare_clean_launch = prepare\n"
                "runner.local_state.convert_clean_handoff_to_validator_lease = convert\n"
                "runner.local_state.restore = restore\n"
                "raise SystemExit(runner.main())\n",
                encoding="utf-8",
            )

            env = os.environ.copy()
            for name in (
                "YIELD_CONFIG_HOME",
                "YIELD_CONFIG_PROFILE",
                "YIELD_LOCAL_CI_COORDINATOR_FD",
                "YIELD_MAINTENANCE_ADMISSION_FD",
                "YIELD_MAINTENANCE_TRANSITION_FD",
            ):
                env.pop(name, None)
            env["YIELD_GPUI_CI_CHECKOUT"] = str(checkout)
            env["YIELD_TEST_CHILD"] = str(child)
            process = subprocess.Popen(
                [sys.executable, str(wrapper)],
                cwd=root,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not (
                    child_ready.exists() and runner_handler_ready.exists()
                ) and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                if not (child_ready.exists() and runner_handler_ready.exists()):
                    early_output, _ = process.communicate(timeout=2)
                    self.fail(
                        "full validation did not reach signal-ready points: "
                        + early_output.decode("utf-8", errors="replace")
                    )

                os.killpg(process.pid, signal.SIGINT)
                output, _ = process.communicate(timeout=10)

                decoded = output.decode("utf-8", errors="replace")
                self.assertEqual(process.returncode, 128 + signal.SIGINT, decoded)
                self.assertTrue(child_exited.exists(), "full-validation child did not handle SIGINT")
                self.assertTrue(restored.exists(), "production state was not restored")
                self.assertIn("fake validator output after SIGINT", decoded)
                self.assertIn("'restored': True", decoded)
            finally:
                # This test alone created the session, so its PGID is safe to
                # clean even if the wrapper exited while a child retained stdout.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if process.poll() is None:
                    process.communicate(timeout=5)


if __name__ == "__main__":
    unittest.main()
