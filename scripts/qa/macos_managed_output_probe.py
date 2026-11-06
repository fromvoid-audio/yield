"""Disposable APFS ownership probe. No installed helper or library access."""

import contextlib
import ctypes
import errno
import os
from pathlib import Path
import plistlib
import pwd
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import wave


PARENT = Path("/private/tmp")
PREFIX = "yield-managed-apfs."
DIRECTORY = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
READ_FILE = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
CREATE_FILE = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
STAGED = "owned.wav"
PUBLISHED = "clip.wav"
PCM = struct.pack("<2h", 123, -456)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def open_fd(stack, path, flags, *, parent=None, mode=0o600):
    fd = os.open(path, flags, mode, dir_fd=parent)
    stack.callback(os.close, fd)
    return fd


def require_no_acl(fd):
    # Darwin sys/acl.h. An absent extended ACL is reported as ENOENT.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.acl_get_fd_np.argtypes = (ctypes.c_int, ctypes.c_int)
    libc.acl_get_fd_np.restype = ctypes.c_void_p
    libc.acl_free.argtypes = (ctypes.c_void_p,)
    ctypes.set_errno(0)
    acl = libc.acl_get_fd_np(fd, 0x100)
    if acl:
        libc.acl_free(acl)
        raise RuntimeError("fixture requires an absent extended ACL")
    require(ctypes.get_errno() == errno.ENOENT, "could not verify absence of an ACL")


def require_dir(fd, uid, permissions):
    info = os.fstat(fd)
    require(info.st_uid == uid, "unexpected directory owner")
    require(stat.S_IMODE(info.st_mode) == permissions, "unexpected directory permissions")
    require_no_acl(fd)


def open_fixture(stack, path, writer_uid):
    require(path.parent == PARENT, "fixture must be under /private/tmp")
    require(
        re.fullmatch(re.escape(PREFIX) + r"[A-Za-z0-9_]{8}", path.name),
        "unexpected fixture name",
    )
    system = open_fd(stack, "/", DIRECTORY)
    require_dir(system, 0, 0o755)
    private = open_fd(stack, "private", DIRECTORY, parent=system)
    require_dir(private, 0, 0o755)
    parent = open_fd(stack, "tmp", DIRECTORY, parent=private)
    require_dir(parent, 0, 0o1777)
    root = open_fd(stack, path.name, DIRECTORY, parent=parent)
    require_dir(root, writer_uid, 0o755)
    return parent, root


def require_apfs(path):
    mount = subprocess.check_output(["/bin/df", "-P", str(path)], timeout=5, text=True)
    device = mount.splitlines()[1].split()[0]
    require(re.fullmatch(r"/dev/disk[0-9]+s[0-9]+", device), "fixture needs a local disk")
    raw_info = subprocess.check_output(
        ["/usr/sbin/diskutil", "info", "-plist", device], timeout=5
    )
    info = plistlib.loads(raw_info)
    require(info.get("FilesystemType") == "apfs", "fixture needs APFS")
    return device


def identity(info):
    require(stat.S_ISREG(info.st_mode), "fixture output must be a regular file")
    require(info.st_nlink == 1, "fixture output must have exactly one link")
    return info.st_dev, info.st_ino


def named_identity(parent, name):
    return identity(os.stat(name, dir_fd=parent, follow_symlinks=False))


def rename_exclusive(source, target):
    # Darwin sys/stdio.h. RENAME_EXCL preserves an existing target.
    rename = ctypes.CDLL(None, use_errno=True).renameatx_np
    rename.argtypes = (
        ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint
    )
    if rename(source, os.fsencode(STAGED), target, os.fsencode(PUBLISHED), 0x4):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def staged_wav(stack, stage, pcm):
    fd = open_fd(stack, STAGED, CREATE_FILE, parent=stage)
    with os.fdopen(os.dup(fd), "wb") as handle, wave.open(handle, "wb") as writer:
        writer.setparams((1, 2, 48_000, 2, "NONE", "not compressed"))
        writer.writeframes(pcm)
    os.fchmod(fd, 0o444)
    os.fsync(fd)
    return fd


def require_payload(final, published_identity=None):
    with contextlib.ExitStack() as stack:
        output = open_fd(stack, PUBLISHED, READ_FILE, parent=final)
        info = os.fstat(output)
        observed_identity = identity(info)
        if published_identity is not None:
            require(observed_identity == published_identity, "reopened final identity differs")
        require(stat.S_IMODE(info.st_mode) == 0o444, "published media is not read-only")
        require_no_acl(output)
        with os.fdopen(os.dup(output), "rb") as handle, wave.open(handle, "rb") as reader:
            layout = (
                reader.getnchannels(),
                reader.getsampwidth(),
                reader.getframerate(),
                reader.getnframes(),
            )
            require(layout == (1, 2, 48_000, 2), "published WAV layout differs")
            require(reader.readframes(2) == PCM, "published PCM differs")


def writer(path, uid, writer_only):
    os.chmod(path, 0o755)
    for name, mode in [("stage", 0o700), ("final", 0o755)]:
        (path / name).mkdir(mode)
        (path / name).chmod(mode)
    device = require_apfs(path)
    with contextlib.ExitStack() as stack:
        _, root = open_fixture(stack, path, uid)
        stage = open_fd(stack, "stage", DIRECTORY, parent=root)
        final = open_fd(stack, "final", DIRECTORY, parent=root)
        require_dir(stage, uid, 0o700)
        require_dir(final, uid, 0o755)
        require(
            os.fstat(root).st_dev == os.fstat(stage).st_dev == os.fstat(final).st_dev,
            "fixture roots must share one device",
        )
        staged = staged_wav(stack, stage, PCM)
        original = identity(os.fstat(staged))
        rename_exclusive(stage, final)
        require_payload(final, original)
        require(identity(os.fstat(staged)) == original, "held stage identity changed")

        collision = staged_wav(stack, stage, struct.pack("<2h", 1, 2))
        retained = identity(os.fstat(collision))
        try:
            rename_exclusive(stage, final)
        except FileExistsError:
            pass
        else:
            raise RuntimeError("exclusive rename unexpectedly replaced final")
        require_payload(final, original)
        require(named_identity(stage, STAGED) == retained, "collision changed the owned stage")

        # Cleanup is checked independently inside the sole-writer directory.
        os.unlink(STAGED, dir_fd=stage)
        require(os.fstat(collision).st_nlink == 0, "cleanup did not remove the held stage")
        require(not os.listdir(stage), "private stage directory is not empty")
        require_payload(final, original)
        print(
            f"Writer UID {uid} on {device}: identity, collision, cleanup and WAV checks passed",
            flush=True,
        )

        if writer_only:
            print("Writer-only mode. Distinct-identity access checks were not run.")
            return
        helper = path / "probe.py"
        shutil.copyfile(Path(__file__).resolve(), helper)
        helper.chmod(0o444)
        subprocess.run(
            ["/usr/bin/sudo", "-n", "-u", "nobody", "--", "/usr/bin/python3", "-I",
             str(helper), "--peer", str(uid), str(path)],
            check=True,
            timeout=10,
        )
        require_payload(final, original)


def require_denied(attempt, message):
    try:
        attempt()
    except PermissionError:
        return
    raise RuntimeError(message)


def peer(path, writer_uid):
    uid = os.geteuid()
    require(
        uid == pwd.getpwnam("nobody").pw_uid and uid != 0 and uid != writer_uid,
        "peer must be the distinct unprivileged fixture account",
    )
    with contextlib.ExitStack() as stack:
        parent, root = open_fixture(stack, path, writer_uid)
        final = open_fd(stack, "final", DIRECTORY, parent=root)
        require_dir(final, writer_uid, 0o755)
        require_payload(final)
        require_denied(
            lambda: open_fd(stack, "stage", DIRECTORY, parent=root),
            "peer opened private stage",
        )
        require_denied(
            lambda: open_fd(stack, PUBLISHED, os.O_WRONLY | os.O_NOFOLLOW, parent=final),
            "peer opened final WAV for writing",
        )
        require_denied(lambda: os.unlink(PUBLISHED, dir_fd=final), "peer removed final WAV")
        require_denied(
            lambda: os.rename(PUBLISHED, "peer.wav", src_dir_fd=final, dst_dir_fd=final),
            "peer renamed final WAV",
        )
        require_denied(
            lambda: open_fd(stack, "peer.wav", CREATE_FILE, parent=final),
            "peer created a replacement candidate in final",
        )
        for name in ("stage", "final"):
            require_denied(
                lambda: os.rename(name, "peer-dir", src_dir_fd=root, dst_dir_fd=root),
                f"peer renamed protected {name} directory",
            )
        # Check removal authority from the root's parent using the sticky-directory rule.
        moved = path.name + ".peer"
        try:
            os.rename(path.name, moved, src_dir_fd=parent, dst_dir_fd=parent)
        except PermissionError:
            pass
        else:
            os.rename(moved, path.name, src_dir_fd=parent, dst_dir_fd=parent)
            raise RuntimeError("peer renamed the fixture root")
        require_payload(final)
    print(f"Peer UID {uid}: published WAV readable and namespace mutations refused")


def main(args):
    require(sys.platform == "darwin", "this fixture requires macOS")
    if len(args) == 3 and args[0] == "--peer":
        peer(Path(args[2]), int(args[1]))
    else:
        require(
            args in ([], ["--writer-only"]),
            "usage: macos_managed_output_probe.py [--writer-only]",
        )
        uid = os.geteuid()
        require(uid != 0, "run the writer fixture as your ordinary account")
        with tempfile.TemporaryDirectory(prefix=PREFIX, dir=PARENT) as fixture:
            writer(Path(fixture), uid, bool(args))


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"Managed APFS probe incomplete: {error}", file=sys.stderr)
        sys.exit(1)
