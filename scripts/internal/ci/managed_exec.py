#!/usr/bin/env python3
"""Run a command in a cancellable process group while inheriting CI lock custody."""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from process_tree import (  # noqa: E402
    Cancelled,
    GroupCleanupUnconfirmed,
    catch_cancellation_signals,
    group_exists,
    ignore_cancellation_signals,
    terminate_owned_group,
)

# The full runner may need 40 seconds to join its nested validator. Allow 10
# more before SIGTERM and another 5 before the final SIGKILL.
OUTER_CLEANUP_GRACE_SECONDS = (40.0, 10.0, 5.0)


def main() -> int:
    if len(sys.argv) < 3 or sys.argv[1] != "--timeout-seconds":
        print("usage: managed_exec.py --timeout-seconds <seconds> <command> [args...]", file=sys.stderr)
        return 2
    try:
        timeout = int(sys.argv[2])
        if timeout <= 0:
            raise ValueError
    except ValueError:
        print("timeout must be a positive integer", file=sys.stderr)
        return 2
    command = sys.argv[3:]
    if not command:
        print("command is required", file=sys.stderr)
        return 2
    pass_fds: tuple[int, ...] = ()
    raw_fd = os.environ.get("YIELD_LOCAL_CI_COORDINATOR_FD")
    if raw_fd is not None:
        try:
            fd = int(raw_fd)
            os.fstat(fd)
            pass_fds = (fd,)
        except (ValueError, OSError):
            print("inherited CI coordinator descriptor is invalid", file=sys.stderr)
            return 2
    with catch_cancellation_signals(defer_startup=True) as activate_cancellation:
        process = None
        try:
            process = subprocess.Popen(command, pass_fds=pass_fds, start_new_session=True)
            activate_cancellation()
            status = process.wait(timeout=timeout)
            if group_exists(process.pid):
                ignore_cancellation_signals()
                if not terminate_owned_group(
                    process.pid,
                    process,
                    grace_seconds=OUTER_CLEANUP_GRACE_SECONDS,
                ):
                    raise GroupCleanupUnconfirmed(
                        f"could not confirm exit of managed command process group {process.pid} after leader exit"
                    )
                print(
                    f"managed command left descendants in process group {process.pid}; they were terminated",
                    file=sys.stderr,
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
                grace_seconds=OUTER_CLEANUP_GRACE_SECONDS,
            ):
                print(
                    f"could not confirm exit of managed command process group {process.pid}",
                    file=sys.stderr,
                )
                return 1
            return 128 + cancelled.signum
        except subprocess.TimeoutExpired:
            ignore_cancellation_signals()
            if not terminate_owned_group(
                process.pid,
                process,
                grace_seconds=OUTER_CLEANUP_GRACE_SECONDS,
            ):
                print(
                    f"could not confirm exit of timed-out command process group {process.pid}",
                    file=sys.stderr,
                )
                return 1
            print(f"command exceeded watchdog of {timeout} seconds", file=sys.stderr)
            return 124
        except GroupCleanupUnconfirmed as exc:
            print(f"managed command cleanup remains unconfirmed: {exc}", file=sys.stderr)
            return 1
        except BaseException:
            ignore_cancellation_signals()
            if process is None:
                raise
            if not terminate_owned_group(
                process.pid,
                process,
                grace_seconds=OUTER_CLEANUP_GRACE_SECONDS,
            ):
                print(
                    f"could not confirm exit of failed managed command process group {process.pid}",
                    file=sys.stderr,
                )
                return 1
            raise


if __name__ == "__main__":
    raise SystemExit(main())
