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
    catch_cancellation_signals,
    ignore_cancellation_signals,
    terminate_owned_group,
)


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
    with catch_cancellation_signals():
        process = subprocess.Popen(command, pass_fds=pass_fds, start_new_session=True)
        try:
            return process.wait(timeout=timeout)
        except Cancelled as cancelled:
            ignore_cancellation_signals()
            terminate_owned_group(process.pid, process)
            return 128 + cancelled.signum
        except subprocess.TimeoutExpired:
            ignore_cancellation_signals()
            terminate_owned_group(process.pid, process)
            print(f"command exceeded watchdog of {timeout} seconds", file=sys.stderr)
            return 124


if __name__ == "__main__":
    raise SystemExit(main())
