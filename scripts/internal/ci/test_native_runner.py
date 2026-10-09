"""Evidence checks for the isolated native production selector runner."""

from __future__ import annotations

import importlib.util
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest


RUNNER = pathlib.Path(__file__).with_name("run_native_production_case.py")
SPEC = importlib.util.spec_from_file_location("yield_native_runner", RUNNER)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class NativeRunnerEvidenceTests(unittest.TestCase):
    def test_full_native_selector_allowlist_covers_all_seven_isolated_cases(self) -> None:
        self.assertEqual(
            set(MODULE.CASES),
            {
                "capacity",
                "control",
                "namespace",
                "recording-slot-currentness",
                "recording-lost-reply",
                "chunk-issuance",
                "provider-reconciliation",
            },
        )
        self.assertEqual(len(set(MODULE.CASES.values())), 7)

    def test_accepts_one_exact_successful_selector(self) -> None:
        selector = MODULE.CASES["capacity"]
        output = (
            "running 1 test\n"
            f"test {selector} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.10s\n"
        )
        self.assertTrue(MODULE.selector_output_is_exactly_one_success(selector, output))

    def test_rejects_empty_skipped_failed_or_different_selector_output(self) -> None:
        selector = MODULE.CASES["capacity"]
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(selector, ""))
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(
            selector,
            "running 1 test\n"
            f"test {selector} ... ignored\n"
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 82 filtered out; finished in 0.00s\n",
        ))
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(
            selector,
            "running 1 test\n"
            f"test {MODULE.CASES['control']} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.00s\n",
        ))

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups and signals")
    def test_cancellation_waits_for_owned_cargo_before_restoring_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-native-runner-cancel-") as temporary:
            root = pathlib.Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            ready = root / "cargo-ready"
            handler_ready = root / "runner-handler-ready"
            child_exited = root / "cargo-exited"
            restored = root / "state-restored"
            cargo = bin_dir / "cargo"
            cargo.write_text(
                "#!" + sys.executable + "\n"
                "import os, pathlib, signal, sys, time\n"
                f"ready = pathlib.Path({str(ready)!r})\n"
                f"exited = pathlib.Path({str(child_exited)!r})\n"
                "def stop(_signum, _frame):\n"
                "    print('fake cargo output after cancellation', flush=True)\n"
                "    exited.write_text('child exited')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, stop)\n"
                "ready.write_text('ready')\n"
                "while True:\n"
                "    time.sleep(1)\n",
                encoding="utf-8",
            )
            cargo.chmod(0o755)

            wrapper = root / "invoke_runner.py"
            wrapper.write_text(
                "import os, pathlib, sys\n"
                f"sys.path.insert(0, {str(RUNNER.parent)!r})\n"
                "import run_native_production_case as runner\n"
                "from contextlib import contextmanager\n"
                "import time\n"
                f"child_exited = pathlib.Path({str(child_exited)!r})\n"
                f"cargo_ready = pathlib.Path({str(ready)!r})\n"
                f"restored = pathlib.Path({str(restored)!r})\n"
                f"handler_ready = pathlib.Path({str(handler_ready)!r})\n"
                "original_catch = runner.catch_cancellation_signals\n"
                "original_popen = runner.subprocess.Popen\n"
                "cargo_processes = []\n"
                "@contextmanager\n"
                "def tracked_catch():\n"
                "    with original_catch():\n"
                "        handler_ready.write_text('handler installed')\n"
                "        yield\n"
                "def tracked_popen(*args, **kwargs):\n"
                "    process = original_popen(*args, **kwargs)\n"
                "    cargo_processes.append(process)\n"
                "    deadline = time.monotonic() + 5\n"
                "    while not cargo_ready.exists() and process.poll() is None and time.monotonic() < deadline:\n"
                "        time.sleep(0.01)\n"
                "    if not cargo_ready.exists():\n"
                "        raise RuntimeError('fake Cargo did not become ready')\n"
                "    return process\n"
                "def prepare(_namespace, artifact=None):\n"
                "    return ({'transaction': 'isolated-test-transaction'}, "
                "os.open(os.devnull, os.O_RDONLY), os.open(os.devnull, os.O_RDONLY))\n"
                "def restore(_transaction):\n"
                "    assert cargo_processes and cargo_processes[0].returncode is not None, "
                "'state restore began before the Cargo child was reaped'\n"
                "    restored.write_text('restored after Cargo exit')\n"
                "    return {'restored': True}\n"
                "runner.local_state.prepare_clean_launch = prepare\n"
                "runner.local_state.restore = restore\n"
                "runner.catch_cancellation_signals = tracked_catch\n"
                "runner.subprocess.Popen = tracked_popen\n"
                "sys.argv = ['run_native_production_case.py', 'capacity']\n"
                "raise SystemExit(runner.main())\n",
                encoding="utf-8",
            )
            env = os.environ.copy()
            env.pop("YIELD_CONFIG_HOME", None)
            env.pop("YIELD_CONFIG_PROFILE", None)
            env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
            env["YIELD_GPUI_CI_CHECKOUT"] = str(root)
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
                while (
                    not (ready.exists() and handler_ready.exists())
                    and process.poll() is None
                    and time.monotonic() < deadline
                ):
                    time.sleep(0.01)
                if not (ready.exists() and handler_ready.exists()):
                    early_output, _ = process.communicate(timeout=2)
                    self.fail(
                        "runner/Cargo did not reach signal-ready points: "
                        + early_output.decode("utf-8", errors="replace")
                    )
                os.killpg(process.pid, signal.SIGINT)
                output, _ = process.communicate(timeout=10)

                self.assertEqual(process.returncode, 128 + signal.SIGINT, output.decode(errors="replace"))
                self.assertTrue(child_exited.exists(), "owned fake Cargo was not reaped after handling SIGINT")
                self.assertTrue(restored.exists(), "the parked state was not restored")
                decoded = output.decode("utf-8", errors="replace")
                self.assertIn("fake cargo output after cancellation", decoded)
                self.assertIn("native production selector cancelled by signal", decoded)
                self.assertIn('"exit_code": 130', decoded)
                self.assertIn('"selector_execution_verified": false', decoded)
            finally:
                # The wrapper can exit while a defective runner leaves its
                # Cargo child holding the captured-output pipe. This PGID was
                # created exclusively for this test, so clean it even if the
                # group leader has already exited.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if process.poll() is None:
                    process.communicate(timeout=5)


if __name__ == "__main__":
    unittest.main()
