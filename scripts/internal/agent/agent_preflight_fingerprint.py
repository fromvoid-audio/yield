#!/usr/bin/env python3
"""Identify the current worktree inputs; this is check coordination, not a snapshot lock."""
import hashlib
import os
from pathlib import Path
import stat
import subprocess
import sys

root = Path(sys.argv[1]).resolve()


def git(*args):
    return subprocess.check_output(["git", "-C", str(root), *args])

result = hashlib.sha256()


def field(value):
    result.update(len(value).to_bytes(8, "big"))
    result.update(value)

field(os.fsencode(root))
field(git("rev-parse", "HEAD"))
field(git("rev-parse", "--abbrev-ref", "HEAD"))
for variable in (
    "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "RUSTFLAGS", "RUSTDOCFLAGS",
    "RUSTUP_TOOLCHAIN", "CARGO_INCREMENTAL", "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_HOME", "RUSTUP_HOME", "RUSTC", "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER", "PATH",
):
    field(variable.encode())
    field(os.fsencode(os.environ.get(variable, "")))
if len(sys.argv) > 2:
    command = Path(sys.argv[2]).resolve(strict=True)
    field(os.fsencode(sys.argv[2]))
    field(os.fsencode(command))
    field(hashlib.sha256(command.read_bytes()).digest())
paths = sorted(
    set(git("ls-files", "-z", "--cached", "--others", "--exclude-standard").split(b"\0"))
    - {b""}
)
field(str(len(paths)).encode())
for name in paths:
    field(name)
    path = root / os.fsdecode(name)
    try:
        entry = path.lstat()
    except FileNotFoundError:
        field(b"deleted")
        continue
    field(str(stat.S_IMODE(entry.st_mode)).encode())
    if stat.S_ISLNK(entry.st_mode):
        field(b"link")
        field(os.fsencode(os.readlink(path)))
        try:
            entry = path.stat()
        except FileNotFoundError:
            field(b"broken")
            continue
    if not stat.S_ISREG(entry.st_mode):
        raise RuntimeError(f"unsupported preflight input: {path}")
    field(b"regular")
    content = hashlib.sha256()
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        if (before.st_dev, before.st_ino) != (entry.st_dev, entry.st_ino):
            raise RuntimeError(f"preflight input replaced: {path}")
        while block := source.read(65536):
            content.update(block)
        after = os.fstat(source.fileno())
    if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
        after.st_size, after.st_mtime_ns, after.st_ctime_ns
    ):
        raise RuntimeError(f"preflight input changed: {path}")
    field(content.digest())
print(result.hexdigest())
