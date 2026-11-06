#!/usr/bin/env python3
"""Exercise ownership of a disposable macOS watcher subprocess."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time


@dataclass(frozen=True)
class SourceBinding:
    root: Path
    device: int
    inode: int
    generation: int

    @classmethod
    def observe(cls, root: Path, generation: int):
        entry = root.stat()
        return cls(root, entry.st_dev, entry.st_ino, generation)


class ChildSlot:
    """Keep one original handle until the OS confirms and reports its exit."""

    def __init__(self):
        self.child: subprocess.Popen | None = None
        self.binding: SourceBinding | None = None
        self.retiring = False
        self.pending = bytearray()

    def start(self, command: list[str], binding: SourceBinding):
        if self.child is not None:
            raise RuntimeError("previous watcher still owned")
        self.child = subprocess.Popen(command, stdout=subprocess.PIPE, bufsize=0)
        self.binding = binding

    def line(self, deadline: float) -> bytes | None:
        assert self.child is not None and self.child.stdout is not None
        fd = self.child.stdout.fileno()
        while b"\n" not in self.pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
                return None
            data = os.read(fd, 128)
            if not data:
                raise RuntimeError("watcher exited without the expected evidence")
            self.pending.extend(data)
            if len(self.pending) > 256:
                raise RuntimeError("oversized watcher response")
        line, _, self.pending = self.pending.partition(b"\n")
        return bytes(line)

    def receive_ready(self, current: SourceBinding, deadline: float) -> bool:
        reply = self.line(deadline)
        if reply is None or time.monotonic() >= deadline:
            self.retire()
            return False
        if reply != b"ready":
            raise RuntimeError(f"unexpected readiness response {reply!r}")
        if self.retiring or self.binding != current:
            self.retire()
            return False
        return True

    def retire(self):
        assert self.child is not None
        self.retiring = True
        self.child.terminate()

    def reap(self, *, kill: bool) -> int:
        assert self.child is not None and self.child.stdout is not None
        if kill:
            self.child.kill()
        status = self.child.wait(timeout=1)
        self.child.stdout.close()
        self.child = None
        self.binding = None
        self.retiring = False
        self.pending.clear()
        return status


@contextmanager
def owned_slot():
    slot = ChildSlot()
    try:
        yield slot
    finally:
        if slot.child is not None:
            slot.reap(kill=True)


def expect_evidence(slot: ChildSlot, *expected: bytes):
    remaining = set(expected)
    deadline = time.monotonic() + 2
    while remaining:
        evidence = slot.line(deadline)
        if evidence is None:
            raise RuntimeError(f"missing native evidence {remaining!r}")
        if evidence not in expected:
            raise RuntimeError(f"unexpected native evidence {evidence!r}")
        remaining.discard(evidence)


def qualify(executable: Path):
    with tempfile.TemporaryDirectory(prefix="yield-watcher-supervision.", dir="/private/tmp") as temp:
        parent = Path(temp)
        source = parent / "source"
        nested = source / "descendant"
        nested.mkdir(parents=True)
        watcher = [str(executable), "--observe", str(parent), str(source)]
        original = SourceBinding.observe(source, 1)
        current = SourceBinding.observe(source, 2)
        with owned_slot() as slot:
            # Stop the child before it can enter the synchronous API. A
            # stopped process cannot handle SIGTERM until continued or killed.
            stall = [
                sys.executable, "-c",
                "import os,signal,sys; os.kill(os.getpid(), signal.SIGSTOP); os.execv(sys.argv[1], sys.argv[1:])",
                *watcher,
            ]
            slot.start(stall, original)
            assert slot.child is not None
            _, stopped = os.waitpid(slot.child.pid, os.WUNTRACED)
            assert os.WIFSTOPPED(stopped)
            begin = time.monotonic()
            assert not slot.receive_ready(original, begin + 0.05)
            elapsed = time.monotonic() - begin
            assert 0.05 <= elapsed < 0.5
            assert slot.retiring and slot.child.poll() is None
            try:
                slot.start(watcher, current)
            except RuntimeError as error:
                assert str(error) == "previous watcher still owned"
            else:
                raise AssertionError("retry escaped while the original child was retiring")
            assert slot.reap(kill=True) in {-signal.SIGKILL, -signal.SIGTERM}
            print(f"forced startup timeout after {elapsed * 1000:.1f} ms, retry refused until reaped")

            slot.start(watcher, original)
            assert not slot.receive_ready(current, time.monotonic() + 2)
            assert slot.retiring and slot.reap(kill=False) == -signal.SIGTERM
            print("late real startup for a superseded generation refused and reaped")

            slot.start(watcher, original)
            assert slot.child is not None and slot.child.stdout is not None
            assert select.select([slot.child.stdout], [], [], 2)[0]
            displaced = parent / "displaced"
            source.rename(displaced)
            source.mkdir()
            replacement = SourceBinding.observe(source, original.generation)
            assert replacement.inode != original.inode
            assert not slot.receive_ready(replacement, time.monotonic() + 2)
            assert slot.retiring and slot.reap(kill=False) == -signal.SIGTERM
            source.rmdir()
            displaced.rename(source)
            print("late real startup for a replaced root refused and reaped")

            slot.start(watcher, current)
            assert slot.receive_ready(current, time.monotonic() + 2)
            (nested / "kick.wav").write_bytes(b"disposable fixture")
            (parent / "unrelated.wav").write_bytes(b"unrelated fixture")
            expect_evidence(slot, b"descendant", b"filtered-sibling")
            source.rename(parent / "moved-source")
            expect_evidence(slot, b"root-changed")
            slot.retire()
            assert slot.reap(kill=False) == -signal.SIGTERM
            print("real descendant and root lifecycle observed, sibling filtered, child reaped")


def main():
    if not __debug__:
        raise SystemExit("watcher qualification requires Python assertions")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=Path, help="compiled macos_watcher_start_probe.c")
    args = parser.parse_args()
    qualify(args.probe.resolve(strict=True))


if __name__ == "__main__":
    main()
