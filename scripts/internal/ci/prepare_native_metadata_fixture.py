#!/usr/bin/env python3
"""Create or verify the persistent source used by native metadata replay tests."""

from __future__ import annotations

import argparse
import ctypes
import errno
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys
import uuid
from typing import Any


SCHEMA = 1
SOURCE_RELATIVE = ("tests", "fixtures", "native-metadata")
FILES = ("Library/.DS_Store", "stable.wav")
MAX_FIXTURE_FILE_BYTES = 1024 * 1024
MAX_MANIFEST_BYTES = 64 * 1024
DIR_FLAGS = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
READ_FLAGS = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
WRITE_FLAGS = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
RENAME_EXCL = 0x00000004


class FixtureError(RuntimeError):
    pass


def _rename_noreplace(src_fd: int, src_name: str, dst_fd: int, dst_name: str) -> None:
    """Atomically publish a staged directory only if its destination is absent."""
    if sys.platform != "darwin":
        raise FixtureError("atomic no-replace fixture publication is supported only on macOS")
    renameatx_np = getattr(ctypes.CDLL(None, use_errno=True), "renameatx_np", None)
    if renameatx_np is None:
        raise FixtureError("this macOS runtime does not provide renameatx_np; refusing unsafe publication")
    renameatx_np.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    renameatx_np.restype = ctypes.c_int
    result = renameatx_np(src_fd, os.fsencode(src_name), dst_fd, os.fsencode(dst_name), RENAME_EXCL)
    if result != 0:
        error = ctypes.get_errno()
        if error == errno.EEXIST:
            raise FixtureError("fixture destination appeared during preparation; preserving staging tree")
        raise OSError(error, os.strerror(error), dst_name)


def _identity(info: os.stat_result) -> dict[str, int]:
    return {"device": info.st_dev, "inode": info.st_ino}


def _check_directory_info(
    info: os.stat_result,
    label: str,
    *,
    owner: int | None = None,
    mode: int | None = None,
) -> os.stat_result:
    if not stat.S_ISDIR(info.st_mode):
        raise FixtureError(f"fixture path is not a real directory: {label}")
    if owner is not None and info.st_uid != owner:
        raise FixtureError(f"fixture directory is not owned by the current user: {label}")
    actual_mode = stat.S_IMODE(info.st_mode)
    if mode is not None and actual_mode != mode:
        raise FixtureError(f"fixture directory has unexpected mode {actual_mode:04o}: {label}")
    return info


def _check_open_directory_entry(parent_fd: int, name: str, child_fd: int, label: str) -> os.stat_result:
    opened = os.fstat(child_fd)
    try:
        entry = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as exc:
        raise FixtureError(f"fixture directory entry changed while open: {label}") from exc
    _check_directory_info(entry, label)
    if (opened.st_dev, opened.st_ino) != (entry.st_dev, entry.st_ino):
        raise FixtureError(f"fixture directory entry changed while open: {label}")
    return opened


def _open_directory_at(
    parent_fd: int,
    name: str,
    label: str,
    *,
    owner: int | None = os.geteuid(),
    mode: int | None = None,
) -> int:
    try:
        child_fd = os.open(name, DIR_FLAGS, dir_fd=parent_fd)
    except OSError as exc:
        raise FixtureError(f"cannot safely open fixture directory {label}: {exc}") from exc
    try:
        info = _check_directory_info(os.fstat(child_fd), label, owner=owner, mode=mode)
        entry = _check_open_directory_entry(parent_fd, name, child_fd, label)
        if (info.st_dev, info.st_ino) != (entry.st_dev, entry.st_ino):
            raise FixtureError(f"fixture directory entry changed while open: {label}")
        return child_fd
    except BaseException:
        os.close(child_fd)
        raise


def _open_absolute_directory(path: pathlib.Path, label: str, *, owner: int | None = None) -> int:
    path = path.absolute()
    if not path.is_absolute():
        raise FixtureError(f"fixture directory path must be absolute: {path}")
    current_fd = os.open(path.anchor, DIR_FLAGS)
    try:
        walked = pathlib.Path(path.anchor)
        for component in path.parts[1:]:
            walked /= component
            next_fd = _open_directory_at(
                current_fd,
                component,
                str(walked),
                owner=None,
            )
            os.close(current_fd)
            current_fd = next_fd
        _check_directory_info(os.fstat(current_fd), label, owner=owner)
        return current_fd
    except BaseException:
        os.close(current_fd)
        raise


def _make_owned_directory(parent_fd: int, name: str, label: str) -> int:
    try:
        os.mkdir(name, mode=0o700, dir_fd=parent_fd)
    except FileExistsError:
        pass
    return _open_directory_at(parent_fd, name, label, mode=0o700)


def _check_file_info(
    info: os.stat_result,
    label: str,
    *,
    owner: int,
    mode: int,
    maximum_size: int,
) -> os.stat_result:
    if not stat.S_ISREG(info.st_mode):
        raise FixtureError(f"fixture entry is not a regular file: {label}")
    if info.st_uid != owner or stat.S_IMODE(info.st_mode) != mode:
        raise FixtureError(f"fixture file owner or mode differs from its manifest: {label}")
    if info.st_size < 0 or info.st_size > maximum_size:
        raise FixtureError(f"fixture file exceeds its bounded size: {label}")
    return info


def _read_file_at(
    parent_fd: int,
    name: str,
    label: str,
    *,
    mode: int,
    maximum_size: int = MAX_FIXTURE_FILE_BYTES,
    expected_hash: str | None = None,
) -> tuple[os.stat_result, bytes, str]:
    try:
        fd = os.open(name, READ_FLAGS, dir_fd=parent_fd)
    except OSError as exc:
        raise FixtureError(f"cannot safely open fixture file {label}: {exc}") from exc
    try:
        before = _check_file_info(os.fstat(fd), label, owner=os.geteuid(), mode=mode, maximum_size=maximum_size)
        chunks: list[bytes] = []
        size = 0
        while True:
            chunk = os.read(fd, min(65536, maximum_size + 1 - size))
            if not chunk:
                break
            size += len(chunk)
            if size > maximum_size:
                raise FixtureError(f"fixture file exceeds its bounded size: {label}")
            chunks.append(chunk)
        data = b"".join(chunks)
        after = _check_file_info(os.fstat(fd), label, owner=os.geteuid(), mode=mode, maximum_size=maximum_size)
        try:
            entry = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        except OSError as exc:
            raise FixtureError(f"fixture file entry changed while open: {label}") from exc
        _check_file_info(entry, label, owner=os.geteuid(), mode=mode, maximum_size=maximum_size)
        before_signature = (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns)
        after_signature = (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns)
        entry_signature = (entry.st_dev, entry.st_ino, entry.st_size, entry.st_mtime_ns, entry.st_ctime_ns)
        if before_signature != after_signature or after_signature != entry_signature or len(data) != after.st_size:
            raise FixtureError(f"fixture file changed while it was being read: {label}")
        digest = hashlib.sha256(data).hexdigest()
        if expected_hash is not None and digest != expected_hash:
            raise FixtureError(f"fixture file content differs from its manifest: {label}")
        return after, data, digest
    finally:
        os.close(fd)


def _write_file_at(
    parent_fd: int,
    name: str,
    label: str,
    data: bytes,
    *,
    mode: int,
    maximum_size: int = MAX_FIXTURE_FILE_BYTES,
) -> os.stat_result:
    if len(data) > maximum_size:
        raise FixtureError(f"generated fixture file exceeds its bounded size: {label}")
    try:
        fd = os.open(name, WRITE_FLAGS, mode, dir_fd=parent_fd)
    except OSError as exc:
        raise FixtureError(f"cannot create fixture file {label}: {exc}") from exc
    try:
        os.fchmod(fd, mode)
        offset = 0
        while offset < len(data):
            written = os.write(fd, data[offset:])
            if written <= 0:
                raise FixtureError(f"short write while preparing fixture file: {label}")
            offset += written
        os.fsync(fd)
        info = _check_file_info(os.fstat(fd), label, owner=os.geteuid(), mode=mode, maximum_size=maximum_size)
        entry = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        if (info.st_dev, info.st_ino) != (entry.st_dev, entry.st_ino):
            raise FixtureError(f"fixture file entry changed while it was written: {label}")
        return info
    finally:
        os.close(fd)


def _scan_source_tree(source_fd: int, source_label: str) -> int:
    entries = set(os.listdir(source_fd))
    if entries != {"Library", "stable.wav"}:
        raise FixtureError(f"fixture source has unknown or missing entries: {sorted(entries)}")
    library_fd = _open_directory_at(source_fd, "Library", f"{source_label}/Library")
    try:
        library_entries = set(os.listdir(library_fd))
        if library_entries != {".DS_Store"}:
            raise FixtureError(f"fixture Library has unknown or missing entries: {sorted(library_entries)}")
        return library_fd
    except BaseException:
        os.close(library_fd)
        raise


def _read_source_files(checkout_fd: int, checkout: pathlib.Path) -> tuple[dict[str, Any], dict[str, bytes]]:
    current_fd = os.dup(checkout_fd)
    try:
        for component in ("crates", "yield-catalog", *SOURCE_RELATIVE):
            next_fd = _open_directory_at(current_fd, component, f"{checkout}/{component}")
            os.close(current_fd)
            current_fd = next_fd
        library_fd = _scan_source_tree(current_fd, "tracked source fixture")
        try:
            payloads: dict[str, bytes] = {}
            records = []
            for relative in FILES:
                if relative.startswith("Library/"):
                    parent_fd, name = library_fd, relative.split("/", 1)[1]
                else:
                    parent_fd, name = current_fd, relative
                info, data, digest = _read_file_at(parent_fd, name, f"tracked source fixture/{relative}", mode=0o644)
                if info.st_size != len(data):
                    raise FixtureError(f"tracked fixture length changed while read: {relative}")
                payloads[relative] = data
                records.append({"path": relative, "mode": 0o600, "sha256": digest})
            key_payload = json.dumps(
                {"schema": SCHEMA, "files": records}, sort_keys=True, separators=(",", ":")
            ).encode("utf-8")
            result = {
                "schema": SCHEMA,
                "key": hashlib.sha256(key_payload).hexdigest(),
                "files": records,
            }
            return result, payloads
        finally:
            os.close(library_fd)
    finally:
        os.close(current_fd)


def _directory_identity(fd: int, label: str, *, mode: int) -> dict[str, int]:
    info = _check_directory_info(os.fstat(fd), label, owner=os.geteuid(), mode=mode)
    return _identity(info)


def _verify_keyed_fixture(
    namespace_fd: int,
    keyed_name: str,
    keyed_path: pathlib.Path,
    source_manifest: dict[str, Any],
) -> dict[str, Any]:
    keyed_fd = _open_directory_at(namespace_fd, keyed_name, str(keyed_path), mode=0o700)
    try:
        if set(os.listdir(keyed_fd)) != {"manifest.json", "source"}:
            raise FixtureError("keyed fixture contains unknown or missing entries")
        manifest_info, manifest_bytes, _ = _read_file_at(
            keyed_fd, "manifest.json", str(keyed_path / "manifest.json"), mode=0o600, maximum_size=MAX_MANIFEST_BYTES
        )
        try:
            manifest = json.loads(manifest_bytes.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError) as exc:
            raise FixtureError("keyed fixture manifest is unreadable or malformed") from exc

        source_fd = _open_directory_at(keyed_fd, "source", str(keyed_path / "source"), mode=0o700)
        try:
            library_fd = _scan_source_tree(source_fd, str(keyed_path / "source"))
            try:
                file_identities: dict[str, dict[str, int]] = {}
                for record in source_manifest["files"]:
                    if record["path"].startswith("Library/"):
                        parent_fd, name = library_fd, record["path"].split("/", 1)[1]
                    else:
                        parent_fd, name = source_fd, record["path"]
                    info, _, _ = _read_file_at(
                        parent_fd,
                        name,
                        f"{keyed_path}/source/{record['path']}",
                        mode=0o600,
                        expected_hash=record["sha256"],
                    )
                    file_identities[record["path"]] = _identity(info)
                expected = {
                    "schema": SCHEMA,
                    "key": source_manifest["key"],
                    "uid": os.geteuid(),
                    "root_identity": _directory_identity(keyed_fd, str(keyed_path), mode=0o700),
                    "source_identity": _directory_identity(source_fd, str(keyed_path / "source"), mode=0o700),
                    "files": source_manifest["files"],
                    "file_identities": file_identities,
                }
                expected_bytes = (json.dumps(expected, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
                if manifest != expected or manifest_bytes != expected_bytes:
                    raise FixtureError("existing keyed fixture identity or manifest differs; refusing to repair it")
                if manifest_info.st_size != len(manifest_bytes):
                    raise FixtureError("fixture manifest changed while it was read")
                _check_open_directory_entry(namespace_fd, keyed_name, keyed_fd, str(keyed_path))
                return {
                    "key": source_manifest["key"],
                    "manifest": str(keyed_path / "manifest.json"),
                    "source_root": str(keyed_path / "source"),
                }
            finally:
                os.close(library_fd)
        finally:
            os.close(source_fd)
    finally:
        os.close(keyed_fd)


def _write_keyed_fixture(
    namespace_fd: int,
    namespace_path: pathlib.Path,
    source_manifest: dict[str, Any],
    payloads: dict[str, bytes],
) -> pathlib.Path:
    temp_name = f".{source_manifest['key']}.prepare-{uuid.uuid4().hex}"
    try:
        os.mkdir(temp_name, mode=0o700, dir_fd=namespace_fd)
    except OSError as exc:
        raise FixtureError(f"cannot create atomic fixture staging directory: {exc}") from exc
    temp_path = namespace_path / temp_name
    temp_fd = _open_directory_at(namespace_fd, temp_name, str(temp_path), mode=0o700)
    try:
        os.mkdir("source", mode=0o700, dir_fd=temp_fd)
        source_fd = _open_directory_at(temp_fd, "source", str(temp_path / "source"), mode=0o700)
        try:
            os.mkdir("Library", mode=0o700, dir_fd=source_fd)
            library_fd = _open_directory_at(source_fd, "Library", str(temp_path / "source/Library"), mode=0o700)
            try:
                identities: dict[str, dict[str, int]] = {}
                for record in source_manifest["files"]:
                    relative = record["path"]
                    parent_fd = library_fd if relative.startswith("Library/") else source_fd
                    name = relative.split("/", 1)[1] if relative.startswith("Library/") else relative
                    info = _write_file_at(
                        parent_fd,
                        name,
                        f"{temp_path}/source/{relative}",
                        payloads[relative],
                        mode=0o600,
                    )
                    identities[relative] = _identity(info)
                os.fsync(library_fd)
                os.fsync(source_fd)
                manifest = {
                    "schema": SCHEMA,
                    "key": source_manifest["key"],
                    "uid": os.geteuid(),
                    "root_identity": _directory_identity(temp_fd, str(temp_path), mode=0o700),
                    "source_identity": _directory_identity(source_fd, str(temp_path / "source"), mode=0o700),
                    "files": source_manifest["files"],
                    "file_identities": identities,
                }
                manifest_bytes = (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
                _write_file_at(
                    temp_fd,
                    "manifest.json",
                    str(temp_path / "manifest.json"),
                    manifest_bytes,
                    mode=0o600,
                    maximum_size=MAX_MANIFEST_BYTES,
                )
                os.fsync(temp_fd)
            finally:
                os.close(library_fd)
        finally:
            os.close(source_fd)
        _check_open_directory_entry(namespace_fd, temp_name, temp_fd, str(temp_path))
        return temp_path
    except BaseException:
        # Retain interrupted preparation as evidence. The next run will refuse the
        # unknown entry rather than repairing or deleting it automatically.
        raise
    finally:
        os.close(temp_fd)


def prepare(state_dir: pathlib.Path, checkout: pathlib.Path) -> dict[str, Any]:
    state_dir = state_dir.resolve(strict=True)
    checkout = checkout.resolve(strict=True)
    state_fd = _open_absolute_directory(state_dir, str(state_dir), owner=os.geteuid())
    checkout_fd = _open_absolute_directory(checkout, str(checkout), owner=os.geteuid())
    try:
        source_manifest, payloads = _read_source_files(checkout_fd, checkout)
        fixtures_fd = _make_owned_directory(state_fd, "fixtures", str(state_dir / "fixtures"))
        try:
            namespace_fd = _make_owned_directory(fixtures_fd, "native-metadata", str(state_dir / "fixtures/native-metadata"))
            try:
                key = source_manifest["key"]
                keyed_path = state_dir / "fixtures/native-metadata" / key
                entries = set(os.listdir(namespace_fd))
                if entries - {key}:
                    raise FixtureError(f"unrecognized native metadata fixture entries: {sorted(entries)}")
                try:
                    os.stat(key, dir_fd=namespace_fd, follow_symlinks=False)
                except FileNotFoundError:
                    temp_path = _write_keyed_fixture(
                        namespace_fd, keyed_path.parent, source_manifest, payloads
                    )
                    temp_name = temp_path.name
                    try:
                        os.stat(key, dir_fd=namespace_fd, follow_symlinks=False)
                    except FileNotFoundError:
                        _rename_noreplace(namespace_fd, temp_name, namespace_fd, key)
                        os.fsync(namespace_fd)
                    else:
                        raise FixtureError("fixture destination appeared during preparation; preserving staging tree")
                result = _verify_keyed_fixture(namespace_fd, key, keyed_path, source_manifest)
                _check_open_directory_entry(fixtures_fd, "native-metadata", namespace_fd, str(keyed_path.parent))
                return result
            finally:
                os.close(namespace_fd)
        finally:
            _check_open_directory_entry(state_fd, "fixtures", fixtures_fd, str(state_dir / "fixtures"))
            os.close(fixtures_fd)
    finally:
        os.close(state_fd)
        os.close(checkout_fd)


def _git_common_dir(checkout: pathlib.Path) -> pathlib.Path:
    result = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=checkout,
        check=True,
        capture_output=True,
        text=True,
    )
    return pathlib.Path(result.stdout.strip()).resolve(strict=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkout", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--state-dir", type=pathlib.Path)
    parser.add_argument("--source-root-only", action="store_true")
    args = parser.parse_args(argv)
    try:
        state_dir = args.state_dir or (_git_common_dir(args.checkout) / "local-ci")
        result = prepare(state_dir, args.checkout)
    except (FixtureError, OSError, subprocess.CalledProcessError) as exc:
        print(f"native metadata fixture preparation failed: {exc}", file=sys.stderr)
        return 2
    if args.source_root_only:
        print(result["source_root"])
    else:
        print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
