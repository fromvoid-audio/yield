#!/usr/bin/env python3
"""Exec a command while holding the common local-CI flock descriptor."""

import os
import pathlib
import stat
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: coordinator_exec.py <lock-path> <command> [args...]", file=sys.stderr)
        return 2
    lock_path = pathlib.Path(sys.argv[1])
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        fd = os.open(
            lock_path,
            os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_CLOEXEC", 0),
            0o600,
        )
    except OSError as error:
        print(f"cannot safely open local-CI coordinator lock: {error}", file=sys.stderr)
        return 1
    metadata = os.fstat(fd)
    entry = lock_path.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(entry.st_mode)
        or not stat.S_ISREG(entry.st_mode)
        or metadata.st_uid != os.geteuid()
        or metadata.st_nlink != 1
        or (metadata.st_dev, metadata.st_ino) != (entry.st_dev, entry.st_ino)
    ):
        os.close(fd)
        print("local-CI lock identity is not a regular current-user-owned file", file=sys.stderr)
        return 1
    os.set_inheritable(fd, True)
    # Python uses the same BSD flock primitive as local_state.py and native admission.
    import fcntl

    fcntl.flock(fd, fcntl.LOCK_EX)
    os.environ["YIELD_LOCAL_CI_COORDINATOR_FD"] = str(fd)
    os.execvp(sys.argv[2], sys.argv[2:])
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
