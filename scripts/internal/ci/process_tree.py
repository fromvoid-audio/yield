"""Cancellation primitives for CI-owned process groups."""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import time
from collections.abc import Iterator


class Cancelled(BaseException):
    def __init__(self, signum: int):
        self.signum = signum


@contextlib.contextmanager
def catch_cancellation_signals() -> Iterator[None]:
    previous = {number: signal.getsignal(number) for number in (signal.SIGINT, signal.SIGTERM)}

    def cancel(signum: int, _frame: object) -> None:
        raise Cancelled(signum)

    for number in previous:
        signal.signal(number, cancel)
    try:
        yield
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)


def ignore_cancellation_signals() -> None:
    """Keep later escalation signals from interrupting the owned cleanup path."""
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)


def group_exists(pgid: int) -> bool:
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def wait_owned_group_exit(
    pgid: int,
    leader: subprocess.Popen[bytes],
    timeout: float,
) -> bool:
    deadline = time.monotonic() + timeout
    while group_exists(pgid):
        leader.poll()  # Reap the leader without mistaking its exit for group exit.
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        time.sleep(min(0.05, remaining))
    leader.poll()
    return True


def terminate_owned_group(
    pgid: int,
    leader: subprocess.Popen[bytes],
    *,
    grace_seconds: tuple[float, float, float] = (10.0, 10.0, 5.0),
) -> None:
    """Escalate against a PGID created with start_new_session by this caller."""
    for signum, grace in zip((signal.SIGINT, signal.SIGTERM, signal.SIGKILL), grace_seconds, strict=True):
        if not group_exists(pgid):
            break
        try:
            os.killpg(pgid, signum)
        except ProcessLookupError:
            break
        if wait_owned_group_exit(pgid, leader, grace):
            break
    try:
        leader.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass
