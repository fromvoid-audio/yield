"""Cancellation primitives for CI-owned process groups."""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import time
from collections.abc import Callable, Iterator

DEFAULT_TERMINATION_GRACE_SECONDS = (10.0, 10.0, 5.0)
LEADER_REAP_TIMEOUT_SECONDS = 1.0


class Cancelled(BaseException):
    def __init__(self, signum: int):
        self.signum = signum


class GroupCleanupUnconfirmed(RuntimeError):
    """Owned process-group cleanup could not be confirmed."""


@contextlib.contextmanager
def catch_cancellation_signals(
    *, defer_startup: bool = False, preserve_ignored_on_exit: bool = False
) -> Iterator[Callable[[], None]]:
    previous = {number: signal.getsignal(number) for number in (signal.SIGINT, signal.SIGTERM)}
    active = not defer_startup
    pending_signum: int | None = None

    def cancel(signum: int, _frame: object) -> None:
        nonlocal pending_signum
        if not active:
            if pending_signum is None:
                pending_signum = signum
            return
        raise Cancelled(signum)

    def activate() -> None:
        nonlocal active, pending_signum
        if pending_signum is not None:
            signum = pending_signum
            pending_signum = None
            raise Cancelled(signum)
        active = True
        if pending_signum is not None:
            signum = pending_signum
            pending_signum = None
            raise Cancelled(signum)

    for number in previous:
        signal.signal(number, cancel)
    try:
        yield activate
    finally:
        for number, handler in previous.items():
            # A nested owned-process scope may finish its process-group join
            # while an outer transaction still needs to restore parked state.
            # Keep escalation signals ignored until that outer scope exits.
            if preserve_ignored_on_exit and signal.getsignal(number) is signal.SIG_IGN:
                continue
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
    grace_seconds: tuple[float, float, float] = DEFAULT_TERMINATION_GRACE_SECONDS,
) -> bool:
    """Escalate and confirm exit of a PGID created with start_new_session."""
    if len(grace_seconds) != 3:
        return False
    try:
        group_exited = not group_exists(pgid)
        for signum, grace in zip((signal.SIGINT, signal.SIGTERM, signal.SIGKILL), grace_seconds):
            if group_exited:
                break
            try:
                os.killpg(pgid, signum)
            except ProcessLookupError:
                group_exited = not group_exists(pgid)
            except PermissionError:
                group_exited = False
            else:
                group_exited = wait_owned_group_exit(pgid, leader, grace)
        leader.wait(timeout=LEADER_REAP_TIMEOUT_SECONDS)
        return group_exited or not group_exists(pgid)
    except BaseException:
        # A signal/OS/wait failure is not evidence that the group exited. The
        # caller must treat this as unconfirmed and preserve any parked state.
        return False
