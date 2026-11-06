"""Cancellation ordering checks for full-validation state transactions."""

from __future__ import annotations

import contextlib
import io
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

CI_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(CI_DIR))

import run_full_ci as runner
from process_tree import GroupCleanupUnconfirmed, terminate_owned_group


class FullCiRunnerArgumentTests(unittest.TestCase):
    def test_help_and_invalid_arguments_exit_before_runtime_effects(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-arguments-") as temporary:
            root = pathlib.Path(temporary)
            script = root / "run_full_ci.py"
            script.write_bytes((CI_DIR / "run_full_ci.py").read_bytes())
            sentinel = root / "runtime-started"
            # Fail on the first runtime import, before it can acquire locks,
            # park state or launch validation. Run the real script entrypoint.
            for module in ("runtime_paths", "local_state", "run_native_production_case", "process_tree"):
                (root / f"{module}.py").write_text(
                    "from pathlib import Path\n"
                    f"Path({str(sentinel)!r}).write_text('runtime started')\n"
                    "raise RuntimeError('CLI reached runtime setup')\n",
                    encoding="utf-8",
                )
            env = os.environ.copy()
            env["TMPDIR"] = str(root / "missing-temp-directory")
            for arguments, expected_status in ((["--help"], 0), (["--unknown"], 2), (["full"], 2)):
                with self.subTest(arguments=arguments):
                    result = subprocess.run(
                        [sys.executable, str(script), *arguments],
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=5,
                    )
                    self.assertEqual(result.returncode, expected_status, result.stdout + result.stderr)
                    self.assertIn("usage:", result.stdout if expected_status == 0 else result.stderr)
                    self.assertFalse(sentinel.exists(), "argument handling reached runtime setup")


class FullCiRunnerCancellationTests(unittest.TestCase):
    def test_process_group_probe_errors_are_not_treated_as_confirmed_exit(self) -> None:
        leader = mock.Mock()
        with mock.patch("process_tree.group_exists", side_effect=OSError("simulated group probe failure")):
            self.assertFalse(terminate_owned_group(12345, leader, grace_seconds=(0.0, 0.0, 0.0)))

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups and signals")
    def test_group_cleanup_precedes_restore_after_leader_exits_on_first_signal(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-cancel-") as temporary:
            root = pathlib.Path(temporary)
            checkout = root / "checkout"
            script_dir = checkout / "scripts"
            script_dir.mkdir(parents=True)
            full_script = script_dir / "gpui-local-ci.sh"
            full_script.write_text('#!/bin/sh\nexec "$YIELD_TEST_SUPERVISOR"\n', encoding="utf-8")

            child_ready = root / "child-ready"
            child_exited = root / "child-exited"
            leader_exited = root / "leader-exited"
            child_pgid = root / "child-pgid"
            handler_ready = root / "runner-handler-ready"
            restored = root / "state-restored"
            child = root / "fake-validator-child"
            child.write_text(
                "#!" + sys.executable + "\n"
                "import pathlib, signal, time\n"
                f"ready = pathlib.Path({str(child_ready)!r})\n"
                f"exited = pathlib.Path({str(child_exited)!r})\n"
                "def stop(_signum, _frame):\n"
                "    print('fake validator descendant stopped by SIGTERM', flush=True)\n"
                "    exited.write_text('child exited')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "ready.write_text('ready')\n"
                "while True: time.sleep(1)\n",
                encoding="utf-8",
            )
            child.chmod(0o755)
            supervisor = root / "fake-validator-supervisor"
            supervisor.write_text(
                "#!" + sys.executable + "\n"
                "import os, pathlib, signal, subprocess, time\n"
                f"child = {str(child)!r}\n"
                f"child_ready = pathlib.Path({str(child_ready)!r})\n"
                f"leader_exited = pathlib.Path({str(leader_exited)!r})\n"
                f"pgid_path = pathlib.Path({str(child_pgid)!r})\n"
                "subprocess.Popen([child], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
                "while not child_ready.exists(): time.sleep(0.01)\n"
                "pgid_path.write_text(str(os.getpgrp()))\n"
                "def stop(_signum, _frame):\n"
                "    leader_exited.write_text('leader exited')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, stop)\n"
                "print('supervisor ready', flush=True)\n"
                "while True: time.sleep(1)\n",
                encoding="utf-8",
            )
            supervisor.chmod(0o755)

            wrapper = root / "invoke_full_ci.py"
            module_dir = pathlib.Path(__file__).resolve().parent
            wrapper.write_text(
                "import os, pathlib, signal, sys\n"
                f"sys.path.insert(0, {str(module_dir)!r})\n"
                "import run_full_ci as runner\n"
                "from contextlib import contextmanager\n"
                f"child_exited = pathlib.Path({str(child_exited)!r})\n"
                f"handler_ready = pathlib.Path({str(handler_ready)!r})\n"
                f"restored = pathlib.Path({str(restored)!r})\n"
                f"pgid_path = pathlib.Path({str(child_pgid)!r})\n"
                "original_run = runner.run\n"
                "original_popen = runner.subprocess.Popen\n"
                "original_catch = runner.catch_cancellation_signals\n"
                "runner.CHILD_TERMINATION_GRACE_SECONDS = (0.25, 0.25, 2.0)\n"
                "children = []\n"
                "def tracked_popen(*args, **kwargs):\n"
                "    process = original_popen(*args, **kwargs)\n"
                "    children.append(process)\n"
                "    return process\n"
                "@contextmanager\n"
                "def tracked_catch(*args, **kwargs):\n"
                "    with original_catch(*args, **kwargs) as activate:\n"
                "        handler_ready.write_text('handler installed before spawn')\n"
                "        yield activate\n"
                "def run_or_skip_python_safety(command, env, pass_fds, **kwargs):\n"
                "    if '-m' in command and 'unittest' in command: return 0\n"
                "    return original_run(command, env, pass_fds, **kwargs)\n"
                "def prepare(_namespace, artifact=None):\n"
                "    return ({'transaction': 'isolated-test-transaction'}, "
                "os.open(os.devnull, os.O_RDONLY), os.open(os.devnull, os.O_RDONLY))\n"
                "def convert(_namespace, namespace_fd, _transition_fd): return namespace_fd\n"
                "def restore(_transaction):\n"
                "    assert children and children[-1].returncode is not None, 'leader not reaped'\n"
                "    assert child_exited.exists(), 'restore preceded descendant exit'\n"
                "    try: os.killpg(int(pgid_path.read_text()), 0)\n"
                "    except ProcessLookupError: pass\n"
                "    else: raise AssertionError('restore preceded whole-group exit')\n"
                "    os.kill(os.getpid(), signal.SIGTERM)\n"
                "    restored.write_text('restored after whole-group exit')\n"
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
            env["YIELD_TEST_SUPERVISOR"] = str(supervisor)
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
                    child_ready.exists() and handler_ready.exists() and child_pgid.exists()
                ) and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                if not (child_ready.exists() and handler_ready.exists() and child_pgid.exists()):
                    early_output, _ = process.communicate(timeout=2)
                    self.fail(
                        "full validation did not reach signal-ready points: "
                        + early_output.decode("utf-8", errors="replace")
                    )

                os.killpg(process.pid, signal.SIGINT)
                output, _ = process.communicate(timeout=10)
                decoded = output.decode("utf-8", errors="replace")

                self.assertEqual(process.returncode, 128 + signal.SIGINT, decoded)
                self.assertTrue(leader_exited.exists(), "supervisor did not exit on initial SIGINT")
                self.assertTrue(child_exited.exists(), "descendant did not handle escalated SIGTERM")
                self.assertTrue(restored.exists(), "production state was not restored")
                self.assertIn("'restored': True", decoded)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if child_pgid.exists():
                    try:
                        os.killpg(int(child_pgid.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if process.poll() is None:
                    process.communicate(timeout=5)

    def test_unconfirmed_cleanup_keeps_transaction_parked(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-parked-") as temporary:
            checkout = pathlib.Path(temporary) / "checkout"
            (checkout / "scripts").mkdir(parents=True)
            (checkout / "scripts/gpui-local-ci.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            restored = mock.Mock()
            calls: list[list[str]] = []

            def fail_full_run(
                command: list[str],
                _env: dict[str, str],
                _fds: tuple[int, ...],
                **_kwargs: object,
            ) -> int:
                calls.append(command)
                if "unittest" in command:
                    return 0
                raise GroupCleanupUnconfirmed("fake group remains live")

            with (
                mock.patch.object(runner.sys, "platform", "darwin"),
                mock.patch.dict(
                    os.environ,
                    {"YIELD_GPUI_CI_CHECKOUT": str(checkout)},
                    clear=True,
                ),
                mock.patch.object(runner, "run", side_effect=fail_full_run),
                mock.patch.object(
                    runner.local_state,
                    "prepare_clean_launch",
                    return_value=(
                        {"transaction": "keep-this-backup"},
                        os.open(os.devnull, os.O_RDONLY),
                        os.open(os.devnull, os.O_RDONLY),
                    ),
                ),
                mock.patch.object(
                    runner.local_state,
                    "convert_clean_handoff_to_validator_lease",
                    side_effect=lambda _namespace, namespace_fd, _transition_fd: namespace_fd,
                ),
                mock.patch.object(runner.local_state, "restore", restored),
                mock.patch.object(runner, "NATIVE_PRODUCTION_CASES", ()),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(runner.main(), 1)

            restored.assert_not_called()
            self.assertIn("state remains parked in transaction keep-this-backup", stderr.getvalue())
            self.assertEqual(len(calls), 2)

    def test_descriptor_close_error_is_reported_without_skipping_safe_restore(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-close-error-") as temporary:
            checkout = pathlib.Path(temporary) / "checkout"
            (checkout / "scripts").mkdir(parents=True)
            (checkout / "scripts/gpui-local-ci.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            descriptors: list[int] = []
            restored = mock.Mock(return_value={"restored": True})
            original_close = os.close
            namespace_fd: list[int] = []

            def prepare(_namespace: str, artifact: str | None = None) -> tuple[dict[str, str], int, int]:
                del artifact
                ns = os.open(os.devnull, os.O_RDONLY)
                transition = os.open(os.devnull, os.O_RDONLY)
                descriptors.extend((ns, transition))
                namespace_fd.append(ns)
                return ({"transaction": "close-error-transaction"}, ns, transition)

            def close_with_one_reported_error(fd: int) -> None:
                original_close(fd)
                if namespace_fd and fd == namespace_fd[0]:
                    namespace_fd.clear()
                    raise OSError("simulated descriptor close report")

            try:
                with (
                    mock.patch.object(runner.sys, "platform", "darwin"),
                    mock.patch.dict(os.environ, {"YIELD_GPUI_CI_CHECKOUT": str(checkout)}, clear=True),
                    mock.patch.object(runner, "run", return_value=0),
                    mock.patch.object(runner.local_state, "prepare_clean_launch", side_effect=prepare),
                    mock.patch.object(
                        runner.local_state,
                        "convert_clean_handoff_to_validator_lease",
                        side_effect=lambda _namespace, ns, _transition: ns,
                    ),
                    mock.patch.object(runner.local_state, "restore", restored),
                    mock.patch.object(runner, "NATIVE_PRODUCTION_CASES", ()),
                    mock.patch.object(runner.os, "close", side_effect=close_with_one_reported_error),
                    contextlib.redirect_stderr(io.StringIO()) as stderr,
                ):
                    self.assertEqual(runner.main(), 1)
            finally:
                for fd in descriptors:
                    try:
                        original_close(fd)
                    except OSError:
                        pass

            restored.assert_called_once_with("close-error-transaction")
            self.assertIn("simulated descriptor close report", stderr.getvalue())

    def test_run_preserves_arguments_environment_pass_fds_and_exit_status(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-run-") as temporary:
            root = pathlib.Path(temporary)
            output_path = root / "child.json"
            inherited_fd = os.open(os.devnull, os.O_RDONLY)
            try:
                code = (
                    "import json, os, pathlib, sys\n"
                    f"path = pathlib.Path({str(output_path)!r})\n"
                    "fd = int(sys.argv[1])\n"
                    "payload = {'argv': sys.argv[2:], 'value': os.environ['YIELD_RUN_TEST_VALUE'], "
                    "'fd_open': os.fstat(fd).st_mode >= 0}\n"
                    "path.write_text(json.dumps(payload))\n"
                    "raise SystemExit(7)\n"
                )
                env = os.environ.copy()
                env["YIELD_RUN_TEST_VALUE"] = "preserved"
                status = runner.run(
                    [sys.executable, "-c", code, str(inherited_fd), "arg with spaces", "tail"],
                    env,
                    (inherited_fd,),
                )
            finally:
                os.close(inherited_fd)

            self.assertEqual(status, 7)
            self.assertEqual(
                json.loads(output_path.read_text(encoding="utf-8")),
                {"argv": ["arg with spaces", "tail"], "value": "preserved", "fd_open": True},
            )

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups and signals")
    def test_normal_leader_exit_joins_owned_descendants(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-normal-exit-") as temporary:
            root = pathlib.Path(temporary)
            ready = root / "descendant-ready"
            exited = root / "descendant-exited"
            child_code = (
                "import pathlib, signal, time\n"
                f"ready = pathlib.Path({str(ready)!r})\n"
                f"exited = pathlib.Path({str(exited)!r})\n"
                "def stop(_signum, _frame):\n"
                "    exited.write_text('joined')\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGINT, stop)\n"
                "ready.write_text('ready')\n"
                "while True: time.sleep(1)\n"
            )
            leader_code = (
                "import pathlib, subprocess, sys, time\n"
                f"ready = pathlib.Path({str(ready)!r})\n"
                "subprocess.Popen([sys.executable, '-c', sys.argv[1]])\n"
                "deadline = time.monotonic() + 5\n"
                "while not ready.exists() and time.monotonic() < deadline: time.sleep(0.01)\n"
                "raise SystemExit(0 if ready.exists() else 9)\n"
            )
            with mock.patch.object(runner, "CHILD_TERMINATION_GRACE_SECONDS", (0.1, 0.1, 0.1)):
                status = runner.run(
                    [sys.executable, "-c", leader_code, child_code],
                    os.environ.copy(),
                    (),
                )

            self.assertEqual(status, 1, "a command leaving a descendant must not report success")
            self.assertTrue(exited.exists(), "descendant did not receive owned-group cleanup")

    def test_full_and_native_commands_preserve_ci_contract_and_restore_order(self) -> None:
        with tempfile.TemporaryDirectory(prefix="yield-full-ci-contract-") as temporary:
            checkout = pathlib.Path(temporary) / "checkout"
            script = checkout / "scripts/gpui-local-ci.sh"
            script.parent.mkdir(parents=True)
            script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            coordinator_fd = os.open(os.devnull, os.O_RDONLY)
            events: list[object] = []
            calls: list[tuple[list[str], dict[str, str], tuple[int, ...]]] = []

            def fake_run(
                command: list[str],
                env: dict[str, str],
                fds: tuple[int, ...],
                **kwargs: object,
            ) -> int:
                calls.append((command, env, fds))
                if "unittest" in command:
                    return 0
                if command == ["bash", str(script), "full"]:
                    events.append("full")
                    self.assertTrue(kwargs.get("preserve_ignored_on_exit"))
                    return 0
                events.append("native")
                return 9

            try:
                with (
                    mock.patch.object(runner.sys, "platform", "darwin"),
                    mock.patch.dict(
                        os.environ,
                        {
                            "YIELD_GPUI_CI_CHECKOUT": str(checkout),
                            "YIELD_LOCAL_CI_COORDINATOR_FD": str(coordinator_fd),
                        },
                        clear=True,
                    ),
                    mock.patch.object(runner, "run", side_effect=fake_run),
                    mock.patch.object(
                        runner.local_state,
                        "prepare_clean_launch",
                        return_value=(
                            {"transaction": "contract-test-transaction"},
                            os.open(os.devnull, os.O_RDONLY),
                            os.open(os.devnull, os.O_RDONLY),
                        ),
                    ),
                    mock.patch.object(
                        runner.local_state,
                        "convert_clean_handoff_to_validator_lease",
                        side_effect=lambda _namespace, namespace_fd, _transition_fd: namespace_fd,
                    ),
                    mock.patch.object(
                        runner.local_state,
                        "restore",
                        side_effect=lambda _transaction: events.append("restore") or {"restored": True},
                    ),
                    mock.patch.object(runner, "NATIVE_PRODUCTION_CASES", ("capacity",)),
                ):
                    self.assertEqual(runner.main(), 9)
            finally:
                os.close(coordinator_fd)

            self.assertEqual(events, ["full", "restore", "native"])
            self.assertEqual(len(calls), 3)
            safety, full, native = calls
            self.assertIn("scripts.internal.ci.test_local_state", safety[0])
            self.assertEqual(full[0], ["bash", str(script), "full"])
            self.assertEqual(
                native[0],
                [sys.executable, str(checkout / "scripts/internal/ci/run_native_production_case.py"), "capacity"],
            )
            for _command, env, _fds in calls[:2]:
                self.assertEqual(env.get("YIELD_CI_PYTHON"), sys.executable)
                self.assertEqual(env.get("YIELD_GPUI_CI_ORCHESTRATED"), "1")
            self.assertEqual(native[1].get("YIELD_LOCAL_CI_COORDINATOR_FD"), str(coordinator_fd))
            self.assertEqual(native[1].get("YIELD_GPUI_CI_CHECKOUT"), str(checkout))
            self.assertIn(coordinator_fd, safety[2])
            self.assertIn(coordinator_fd, full[2])
            self.assertEqual(native[2], (coordinator_fd,))


if __name__ == "__main__":
    unittest.main()
