"""Evidence checks for the isolated native production selector runner."""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


RUNNER = pathlib.Path(__file__).with_name("run_native_production_case.py")
SPEC = importlib.util.spec_from_file_location("yield_native_runner", RUNNER)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class NativeRunnerEvidenceTests(unittest.TestCase):
    def test_full_native_selector_allowlist_covers_all_eight_isolated_cases(self) -> None:
        self.assertEqual(
            set(MODULE.CASES),
            {
                "capacity",
                "control",
                "namespace",
                "namespace-poll",
                "recording-slot-currentness",
                "recording-lost-reply",
                "chunk-issuance",
                "provider-reconciliation",
            },
        )
        self.assertEqual(len(set(MODULE.CASES.values())), 8)

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

    def test_normal_selector_preserves_argv_environment_descriptors_status_and_restore_order(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-native-runner-contract-") as temporary:
            root = pathlib.Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cargo_record = root / "cargo.json"
            cargo = bin_dir / "cargo"
            selector = MODULE.CASES["capacity"]
            cargo.write_text(
                "#!" + sys.executable + "\n"
                "import json, os, pathlib, sys\n"
                "record = pathlib.Path(os.environ['YIELD_TEST_CARGO_RECORD'])\n"
                "fd_names = ('YIELD_MAINTENANCE_ADMISSION_FD', 'YIELD_MAINTENANCE_TRANSITION_FD', "
                "'YIELD_LOCAL_CI_COORDINATOR_FD')\n"
                "payload = {'argv': sys.argv[1:], 'value': os.environ['YIELD_TEST_CARGO_VALUE'], "
                "'fd_open': {name: os.fstat(int(os.environ[name])).st_mode >= 0 for name in fd_names}, "
                "'pgid_is_self': os.getpgrp() == os.getpid()}\n"
                "record.write_text(json.dumps(payload))\n"
                "print('running 1 test')\n"
                f"print('test {selector} ... ok')\n"
                "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.10s')\n",
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            coordinator_fd = os.open(os.devnull, os.O_RDONLY)
            owned_fds: list[int] = []
            processes = []
            events: list[str] = []
            original_popen = MODULE.subprocess.Popen

            def prepare(_namespace: str, artifact: str | None = None) -> tuple[dict[str, str], int, int]:
                del artifact
                namespace_fd = os.open(os.devnull, os.O_RDONLY)
                transition_fd = os.open(os.devnull, os.O_RDONLY)
                owned_fds.extend((namespace_fd, transition_fd))
                return ({"transaction": "normal-selector-transaction"}, namespace_fd, transition_fd)

            def tracked_popen(*args: object, **kwargs: object) -> subprocess.Popen[bytes]:
                process = original_popen(*args, **kwargs)
                processes.append(process)
                return process

            def restore(transaction: str) -> dict[str, bool]:
                self.assertEqual(transaction, "normal-selector-transaction")
                self.assertTrue(processes and processes[0].returncode == 0, "restore preceded Cargo status")
                events.append("restore")
                return {"restored": True}

            expected_args = [
                "test", "--locked", "-p", "yield-library", "--lib", "--no-default-features",
                selector, "--", "--exact", "--test-threads=1",
            ]
            env = {
                "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
                "YIELD_GPUI_CI_CHECKOUT": str(root),
                "YIELD_CONFIG_HOME": "",
                "YIELD_CONFIG_PROFILE": "",
                "YIELD_LOCAL_CI_COORDINATOR_FD": str(coordinator_fd),
                "YIELD_TEST_CARGO_RECORD": str(cargo_record),
                "YIELD_TEST_CARGO_VALUE": "native-env-preserved",
            }
            try:
                with (
                    mock.patch.dict(os.environ, env, clear=True),
                    mock.patch.object(MODULE.local_state, "prepare_clean_launch", side_effect=prepare),
                    mock.patch.object(MODULE.local_state, "restore", side_effect=restore),
                    mock.patch.object(MODULE.subprocess, "Popen", side_effect=tracked_popen),
                    mock.patch.object(sys, "argv", [str(RUNNER), "capacity"]),
                ):
                    self.assertEqual(MODULE.main(), 0)
            finally:
                os.close(coordinator_fd)
                for fd in owned_fds:
                    try:
                        os.close(fd)
                    except OSError:
                        pass

            self.assertEqual(events, ["restore"])
            self.assertEqual(len(processes), 1)
            call = processes[0]
            self.assertEqual(call.args, ["cargo", *expected_args])
            payload = json.loads(cargo_record.read_text(encoding="utf-8"))
            self.assertEqual(payload["argv"], expected_args)
            self.assertEqual(payload["value"], "native-env-preserved")
            self.assertTrue(all(payload["fd_open"].values()))
            self.assertTrue(payload["pgid_is_self"], "Cargo did not start in its own process group")
            self.assertTrue(call.returncode == 0)

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups and signals")
    def test_cancellation_waits_for_owned_cargo_before_restoring_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-native-runner-cancel-") as temporary:
            root = pathlib.Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            ready = root / "cargo-ready"
            child_ready = root / "cargo-child-ready"
            handler_ready = root / "runner-handler-ready"
            child_exited = root / "cargo-exited"
            leader_exited = root / "cargo-leader-exited"
            lease_held = root / "simulated-lease-held"
            cargo_pgid = root / "cargo-pgid"
            startup_signal_sent = root / "startup-signal-sent"
            restored = root / "state-restored"
            child = root / "fake-cargo-child.py"
            child.write_text(
                "#!" + sys.executable + "\n"
                "import pathlib, signal, time\n"
                f"ready = pathlib.Path({str(child_ready)!r})\n"
                f"exited = pathlib.Path({str(child_exited)!r})\n"
                f"lease = pathlib.Path({str(lease_held)!r})\n"
                "def stop(_signum, _frame):\n"
                "    print('fake Cargo descendant stopped by SIGTERM', flush=True)\n"
                "    exited.write_text('child exited')\n"
                "    lease.unlink(missing_ok=True)\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "ready.write_text('ready')\n"
                "lease.write_text('lease held until descendant exit')\n"
                "while True: time.sleep(1)\n",
                encoding="utf-8",
            )
            child.chmod(0o755)
            cargo = bin_dir / "cargo"
            cargo.write_text(
                "#!" + sys.executable + "\n"
                "import os, pathlib, signal, subprocess, sys, time\n"
                f"ready = pathlib.Path({str(ready)!r})\n"
                f"child_ready = pathlib.Path({str(child_ready)!r})\n"
                f"child = {str(child)!r}\n"
                f"leader_exited = pathlib.Path({str(leader_exited)!r})\n"
                f"pgid_path = pathlib.Path({str(cargo_pgid)!r})\n"
                "subprocess.Popen([child])\n"
                "while not child_ready.exists(): time.sleep(0.01)\n"
                "pgid_path.write_text(str(os.getpgrp()))\n"
                "def stop(_signum, _frame):\n"
                "    print('fake Cargo supervisor exited on SIGINT', flush=True)\n"
                "    leader_exited.write_text('leader exited')\n"
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
                "import os, pathlib, signal, sys\n"
                f"sys.path.insert(0, {str(RUNNER.parent)!r})\n"
                "import run_native_production_case as runner\n"
                "from contextlib import contextmanager\n"
                "import time\n"
                f"child_exited = pathlib.Path({str(child_exited)!r})\n"
                f"cargo_ready = pathlib.Path({str(ready)!r})\n"
                f"restored = pathlib.Path({str(restored)!r})\n"
                f"handler_ready = pathlib.Path({str(handler_ready)!r})\n"
                f"child_exited = pathlib.Path({str(child_exited)!r})\n"
                f"leader_exited = pathlib.Path({str(leader_exited)!r})\n"
                f"lease_held = pathlib.Path({str(lease_held)!r})\n"
                f"cargo_pgid = pathlib.Path({str(cargo_pgid)!r})\n"
                f"startup_signal_sent = pathlib.Path({str(startup_signal_sent)!r})\n"
                "original_catch = runner.catch_cancellation_signals\n"
                "original_popen = runner.subprocess.Popen\n"
                "cargo_processes = []\n"
                "runner.CARGO_TERMINATION_GRACE_SECONDS = (0.25, 0.25, 2.0)\n"
                "@contextmanager\n"
                "def tracked_catch(*args, **kwargs):\n"
                "    with original_catch(*args, **kwargs) as activate:\n"
                "        handler_ready.write_text('handler installed')\n"
                "        yield activate\n"
                "def tracked_popen(*args, **kwargs):\n"
                "    assert kwargs.get('start_new_session') is True, 'Cargo lacks an owned session'\n"
                "    process = original_popen(*args, **kwargs)\n"
                "    cargo_processes.append(process)\n"
                "    deadline = time.monotonic() + 5\n"
                "    while not cargo_ready.exists() and process.poll() is None and time.monotonic() < deadline:\n"
                "        time.sleep(0.01)\n"
                "    if not cargo_ready.exists():\n"
                "        raise RuntimeError('fake Cargo did not become ready')\n"
                "    os.kill(os.getpid(), signal.SIGINT)\n"
                "    startup_signal_sent.write_text('signal deferred until Popen returns')\n"
                "    return process\n"
                "def prepare(_namespace, artifact=None):\n"
                "    return ({'transaction': 'isolated-test-transaction'}, "
                "os.open(os.devnull, os.O_RDONLY), os.open(os.devnull, os.O_RDONLY))\n"
                "def restore(_transaction):\n"
                "    assert cargo_processes and cargo_processes[0].returncode is not None, "
                "'state restore began before the Cargo leader was reaped'\n"
                "    assert leader_exited.exists(), 'supervisor did not exit on first SIGINT'\n"
                "    assert child_exited.exists(), 'descendant did not exit on escalated SIGTERM'\n"
                "    assert not lease_held.exists(), 'restore preceded descendant lease release'\n"
                "    try: os.killpg(int(cargo_pgid.read_text()), 0)\n"
                "    except ProcessLookupError: pass\n"
                "    else: raise AssertionError('restore preceded whole Cargo group exit')\n"
                "    os.kill(os.getpid(), signal.SIGTERM)\n"
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
                    not (ready.exists() and handler_ready.exists() and startup_signal_sent.exists())
                    and process.poll() is None
                    and time.monotonic() < deadline
                ):
                    time.sleep(0.01)
                if not (ready.exists() and handler_ready.exists() and startup_signal_sent.exists()):
                    early_output, _ = process.communicate(timeout=2)
                    self.fail(
                        "runner/Cargo did not reach signal-ready points: "
                        + early_output.decode("utf-8", errors="replace")
                    )
                output, _ = process.communicate(timeout=10)

                self.assertEqual(process.returncode, 128 + signal.SIGINT, output.decode(errors="replace"))
                self.assertTrue(child_exited.exists(), "owned fake Cargo was not reaped after handling SIGINT")
                self.assertTrue(leader_exited.exists(), "fake Cargo supervisor did not exit on SIGINT")
                self.assertFalse(lease_held.exists(), "fake Cargo descendant kept the lease after restore")
                self.assertTrue(restored.exists(), "the parked state was not restored")
                decoded = output.decode("utf-8", errors="replace")
                self.assertIn("fake Cargo supervisor exited on SIGINT", decoded)
                self.assertIn("fake Cargo descendant stopped by SIGTERM", decoded)
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
                if cargo_pgid.exists():
                    try:
                        os.killpg(int(cargo_pgid.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if process.poll() is None:
                    process.communicate(timeout=5)


if __name__ == "__main__":
    unittest.main()
