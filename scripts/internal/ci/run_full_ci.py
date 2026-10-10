#!/usr/bin/env python3
"""Run ordinary full validation under a reversible clean-state SH lease."""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from runtime_paths import canonicalize_tmpdir  # noqa: E402

canonicalize_tmpdir()
import local_state  # noqa: E402
from run_native_production_case import CASES as NATIVE_PRODUCTION_CASES  # noqa: E402
from process_tree import (  # noqa: E402
    Cancelled,
    GroupCleanupUnconfirmed,
    catch_cancellation_signals,
    group_exists,
    ignore_cancellation_signals,
    terminate_owned_group,
)

# Native selector children need up to 25 seconds for their Cargo group cleanup.
# This 30+5+5 ladder grants that cleanup before escalating against the runner.
CHILD_TERMINATION_GRACE_SECONDS = (30.0, 5.0, 5.0)


def run(
    command: list[str],
    env: dict[str, str],
    pass_fds: tuple[int, ...],
    *,
    preserve_ignored_on_exit: bool = False,
) -> int:
    with catch_cancellation_signals(
        defer_startup=True, preserve_ignored_on_exit=preserve_ignored_on_exit
    ) as activate_cancellation:
        process = None
        try:
            process = subprocess.Popen(
                command,
                env=env,
                pass_fds=pass_fds,
                start_new_session=True,
            )
            activate_cancellation()
            status = process.wait()
            if group_exists(process.pid):
                # A normal leader exit must not release the parked-state transaction
                # while any descendant in its owned group is still running.
                ignore_cancellation_signals()
                if not terminate_owned_group(
                    process.pid,
                    process,
                    grace_seconds=CHILD_TERMINATION_GRACE_SECONDS,
                ):
                    raise GroupCleanupUnconfirmed(
                        f"could not confirm exit of validation process group {process.pid} after leader exit"
                    )
                print(
                    f"validation command left descendants in process group {process.pid}; they were terminated",
                    file=sys.stderr,
                    flush=True,
                )
                return status or 1
            return status
        except Cancelled as cancelled:
            ignore_cancellation_signals()
            if process is None:
                return 128 + cancelled.signum
            if not terminate_owned_group(
                process.pid,
                process,
                grace_seconds=CHILD_TERMINATION_GRACE_SECONDS,
            ):
                raise GroupCleanupUnconfirmed(
                    f"could not confirm exit of cancelled validation process group {process.pid}"
                )
            return 128 + cancelled.signum
        except GroupCleanupUnconfirmed:
            raise
        except BaseException:
            ignore_cancellation_signals()
            if process is None:
                raise
            if not terminate_owned_group(
                process.pid,
                process,
                grace_seconds=CHILD_TERMINATION_GRACE_SECONDS,
            ):
                raise GroupCleanupUnconfirmed(
                    f"could not confirm exit of failed validation process group {process.pid}"
                )
            raise


def _main() -> int:
    if os.name != "posix" or sys.platform != "darwin":
        print("the full native local-CI lane is macOS-only", file=sys.stderr)
        return 2
    if os.environ.get("YIELD_CONFIG_HOME") or os.environ.get("YIELD_CONFIG_PROFILE"):
        print("clear YIELD_CONFIG_HOME and YIELD_CONFIG_PROFILE before full local CI", file=sys.stderr)
        return 2

    checkout = pathlib.Path(os.environ.get("YIELD_GPUI_CI_CHECKOUT", pathlib.Path(__file__).resolve().parents[3]))
    script = checkout / "scripts/gpui-local-ci.sh"
    if not script.is_file():
        print(f"missing frozen full-CI script: {script}", file=sys.stderr)
        return 2
    env = os.environ.copy()
    env["YIELD_CI_PYTHON"] = sys.executable
    env.pop("YIELD_MAINTENANCE_ADMISSION_FD", None)
    env.pop("YIELD_MAINTENANCE_TRANSITION_FD", None)
    env["YIELD_GPUI_CI_ORCHESTRATED"] = "1"
    env["YIELD_GPUI_SKIP_NATIVE_CASES"] = "1"
    namespace_fd = transition_fd = None
    transaction = None
    status = 1
    failure: BaseException | None = None
    keep_transaction_parked = False
    try:
        # These tests use private temporary roots and exercise reset, lock
        # custody and crash recovery without touching Yield's real state. Run
        # them before the first production clean transaction.
        python_safety = [
            env["YIELD_CI_PYTHON"],
            "-m",
            "unittest",
            "scripts.internal.ci.test_local_state",
            "scripts.internal.ci.test_coordinator_exec",
            "scripts.internal.ci.test_native_runner",
            "scripts.internal.ci.test_namespace_window_routing",
            "scripts.internal.ci.test_full_ci_runner",
            "scripts.internal.ci.test_bundle_packager",
            "scripts.internal.ci.test_local_ci_contract",
            "scripts.internal.ci.test_managed_exec",
            "scripts.internal.ci.test_runtime_paths",
            "scripts.internal.release.test_release_wrapper",
            "scripts.internal.ci.test_native_metadata_fixture",
        ]
        coordinator_fds: tuple[int, ...] = ()
        coordinator = env.get("YIELD_LOCAL_CI_COORDINATOR_FD")
        if coordinator is not None:
            coordinator_fd = int(coordinator)
            os.fstat(coordinator_fd)
            coordinator_fds = (coordinator_fd,)
        status = run(python_safety, env, coordinator_fds)
        if status != 0:
            return status
        transaction_result, namespace_fd, transition_fd = local_state.prepare_clean_launch(
            "production", artifact="full-ci:ordinary-suite"
        )
        transaction = transaction_result.get("transaction")
        if transaction is None:
            print("production state is already vanilla; running ordinary validation", flush=True)
        else:
            print(f"parked production state for full validation: {transaction}", flush=True)
        lease_fd = local_state.convert_clean_handoff_to_validator_lease(
            "production", namespace_fd, transition_fd
        )
        os.close(transition_fd)
        transition_fd = None

        inherited = [lease_fd]
        inherited.extend(coordinator_fds)
        status = run(
            ["bash", str(script), "full"],
            env,
            tuple(inherited),
            preserve_ignored_on_exit=True,
        )
    except BaseException as exc:
        failure = exc
        if isinstance(exc, Cancelled):
            ignore_cancellation_signals()
        keep_transaction_parked = isinstance(exc, GroupCleanupUnconfirmed)
        print(f"full validation setup failed: {type(exc).__name__}: {exc}", file=sys.stderr)
    finally:
        if transaction is not None:
            ignore_cancellation_signals()
        descriptor_cleanup_errors: list[str] = []
        for label, fd in (("namespace", namespace_fd), ("transition", transition_fd)):
            if fd is None:
                continue
            try:
                os.close(fd)
            except BaseException as exc:
                descriptor_cleanup_errors.append(f"{label} descriptor close failed: {type(exc).__name__}: {exc}")
                if failure is None:
                    failure = exc
                status = 1
        for error in descriptor_cleanup_errors:
            print(error, file=sys.stderr, flush=True)
        if transaction is not None:
            if keep_transaction_parked:
                print(
                    f"production state remains parked in transaction {transaction}: "
                    "validator process-group exit was not confirmed",
                    file=sys.stderr,
                    flush=True,
                )
            else:
                try:
                    print(local_state.restore(str(transaction)), flush=True)
                except BaseException as exc:
                    failure = exc
                    status = 1
                    print(f"production state restore failed: {type(exc).__name__}: {exc}", file=sys.stderr)
    if failure is not None:
        return 1
    if status != 0:
        return status

    # Each native production selector is a separate clean transaction and exact
    # EX handoff, so durable evidence from one case cannot contaminate another.
    runner = checkout / "scripts/internal/ci/run_native_production_case.py"
    native_env = os.environ.copy()
    native_fds: tuple[int, ...] = ()
    native_fds = coordinator_fds
    for case in NATIVE_PRODUCTION_CASES:
        status = run([sys.executable, str(runner), case], native_env, native_fds)
        if status != 0:
            return status
    return 0


def main() -> int:
    # The transaction restore is part of cancellation cleanup. Keep original
    # handlers saved until _main has completed its full finally block.
    with catch_cancellation_signals():
        return _main()


if __name__ == "__main__":
    raise SystemExit(main())
