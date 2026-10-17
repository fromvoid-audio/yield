"""Crash-recoverable, same-filesystem parking of a Yield app-state root."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import pathlib
import pwd
import re
import stat
import time
import uuid
from contextlib import contextmanager
from typing import Iterator, Optional


CONTROL_NAME = ".yield-local-ci"
NAMESPACES = {"development": ".yield-development", "production": ".yield"}
SCHEMA = 1


class StateError(RuntimeError):
    pass


def canonical_support_dir() -> pathlib.Path:
    if sys_platform() != "darwin":
        raise StateError("local state maintenance is qualified only on macOS")
    home = pathlib.Path(pwd.getpwuid(os.geteuid()).pw_dir)
    support = home / "Library" / "Application Support"
    _validate_owned_dir(home)
    _validate_owned_dir(home / "Library")
    _validate_owned_dir(support)
    return support


def sys_platform() -> str:
    import sys

    return sys.platform


def namespace_paths(namespace: str, support: Optional[pathlib.Path] = None) -> tuple[pathlib.Path, pathlib.Path]:
    try:
        root_name = NAMESPACES[namespace]
    except KeyError as exc:
        raise StateError(f"unknown namespace: {namespace}") from exc
    support = support or canonical_support_dir()
    _validate_owned_dir(support)
    control = support / CONTROL_NAME
    _mkdir_owned(control)
    return support / root_name, control


@contextmanager
def maintenance_locks(control: pathlib.Path, namespace: str, exclusive: bool) -> Iterator[tuple[int, int]]:
    """Acquire transition before namespace, matching the native admission ordering."""
    root_name = NAMESPACES[namespace]
    transition_path = control / f"{root_name}.transition.lock"
    namespace_path = control / f"{root_name}.admission.lock"
    transition_fd = _open_lock(transition_path)
    namespace_fd = _open_lock(namespace_path)
    operation = fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH
    try:
        try:
            fcntl.flock(transition_fd, operation | fcntl.LOCK_NB)
            fcntl.flock(namespace_fd, operation | fcntl.LOCK_NB)
            _validate_regular_owned(transition_path, transition_fd)
            _validate_regular_owned(namespace_path, namespace_fd)
        except BlockingIOError as exc:
            raise StateError("Yield has a live owner or another maintenance operation") from exc
        yield namespace_fd, transition_fd
    finally:
        os.close(namespace_fd)
        os.close(transition_fd)


def status(namespace: str, support: Optional[pathlib.Path] = None) -> dict[str, object]:
    root, control = namespace_paths(namespace, support)
    with maintenance_locks(control, namespace, exclusive=False):
        identity = _identity_if_directory(root)
        return {"namespace": namespace, "root": str(root), "state": "present" if identity else "vanilla", "identity": identity}


def clean(namespace: str, support: Optional[pathlib.Path] = None, artifact: Optional[str] = None, _test_crash_at: Optional[str] = None) -> dict[str, object]:
    root, control = namespace_paths(namespace, support)
    with maintenance_locks(control, namespace, exclusive=True):
        return _clean_locked(root, control, namespace, artifact, _test_crash_at)


def prepare_clean_launch(namespace: str, support: Optional[pathlib.Path] = None, artifact: Optional[str] = None) -> tuple[dict[str, object], int, int]:
    """Park the selected root and return namespace/transition FDs still held EX for handoff."""
    root, control = namespace_paths(namespace, support)
    namespace_fd = _open_lock(control / f"{NAMESPACES[namespace]}.admission.lock")
    transition_fd = _open_lock(control / f"{NAMESPACES[namespace]}.transition.lock")
    try:
        fcntl.flock(transition_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(namespace_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _validate_regular_owned(control / f"{NAMESPACES[namespace]}.transition.lock", transition_fd)
        _validate_regular_owned(control / f"{NAMESPACES[namespace]}.admission.lock", namespace_fd)
        result = _clean_locked(root, control, namespace, artifact)
        return result, namespace_fd, transition_fd
    except BaseException:
        os.close(namespace_fd)
        os.close(transition_fd)
        raise


def convert_clean_handoff_to_validator_lease(
    namespace: str, namespace_fd: int, transition_fd: int, support: Optional[pathlib.Path] = None
) -> int:
    """Safely convert an EX reset handoff to retained SH validator custody.

    The transition lock stays EX until the namespace lock is downgraded. This
    closes flock's non-atomic EX-to-SH window. The returned namespace FD remains
    SH; callers pass it to child processes as custody but do not set the app's
    exclusive-handoff environment variables.
    """
    _root, control = namespace_paths(namespace, support)
    root_name = NAMESPACES[namespace]
    namespace_path = control / f"{root_name}.admission.lock"
    transition_path = control / f"{root_name}.transition.lock"
    _validate_regular_owned(namespace_path, namespace_fd)
    _validate_regular_owned(transition_path, transition_fd)
    probes: list[int] = []
    try:
        for path, handed_fd, label in (
            (transition_path, transition_fd, "transition"),
            (namespace_path, namespace_fd, "namespace"),
        ):
            probe_fd = _open_lock(path)
            probes.append(probe_fd)
            try:
                fcntl.flock(probe_fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise StateError(f"validator handoff {label} lock is not exclusively held")
            try:
                fcntl.flock(handed_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as exc:
                raise StateError(f"validator handoff descriptor does not itself hold the {label} lock") from exc
        # flock conversion is not atomic. Keep transition EX until namespace SH
        # has been established on the same open description.
        fcntl.flock(namespace_fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        fcntl.flock(transition_fd, fcntl.LOCK_UN)
        os.set_inheritable(namespace_fd, True)
        return namespace_fd
    finally:
        for probe_fd in probes:
            os.close(probe_fd)


def _clean_locked(root: pathlib.Path, control: pathlib.Path, namespace: str, artifact: Optional[str], test_crash_at: Optional[str] = None) -> dict[str, object]:
    _refuse_legacy_profile_owners(root)
    _assert_no_unresolved_transactions(control, namespace)
    original = _identity_if_directory(root)
    if original is None:
        return {"namespace": namespace, "state": "already-vanilla", "transaction": None}
    _validate_owned_dir(root)
    backup_root = control / "backups" / NAMESPACES[namespace]
    _mkdir_owned(backup_root.parent)
    _mkdir_owned(backup_root)
    if os.stat(backup_root).st_dev != os.stat(root.parent).st_dev:
        raise StateError("backup and managed root are not on the same filesystem")
    transaction_id = uuid.uuid4().hex
    transaction_dir = backup_root / transaction_id
    transaction_dir.mkdir(mode=0o700)
    _fsync_dir(backup_root)
    record_path = transaction_dir / "transaction.json"
    record: dict[str, object] = {
        "schema": SCHEMA,
        "transaction": transaction_id,
        "namespace": namespace,
        "phase": "prepared",
        "root": str(root),
        "original_identity": original,
        "artifact": artifact,
        "created_unix_ns": time.time_ns(),
    }
    _write_record(record_path, record, _test_crash_at="clean_during_initial_record" if test_crash_at == "clean_during_initial_record" else None)
    _test_crash(test_crash_at, "clean_after_prepare")
    parked = transaction_dir / "original-root"
    if _identity_if_directory(root) != original:
        record["phase"] = "aborted"
        _write_record(record_path, record)
        raise StateError("managed root identity changed before clean rename")
    os.rename(root, parked)
    _fsync_dir(root.parent)
    _fsync_dir(transaction_dir)
    _test_crash(test_crash_at, "clean_after_rename")
    if _identity_if_directory(parked) != original:
        raise StateError("parked original identity changed during clean rename")
    record["phase"] = "original_parked"
    _write_record(record_path, record)
    return {"namespace": namespace, "state": "clean", "transaction": transaction_id, "backup": str(parked), "identity": original}


def _assert_no_unresolved_transactions(control: pathlib.Path, namespace: str) -> None:
    base = control / "backups" / NAMESPACES[namespace]
    if not base.exists():
        return
    _validate_owned_dir(base)
    for tx_dir in base.iterdir():
        _validate_owned_dir(tx_dir)
        record_path = tx_dir / "transaction.json"
        if not record_path.exists():
            raise StateError(
                "transaction preparation has no committed record; explicit recovery is required for "
                + tx_dir.name
            )
        _validate_regular_owned(record_path)
        try:
            record = json.loads(record_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise StateError("an unreadable transaction must be recovered manually") from exc
        if record.get("schema") != SCHEMA or record.get("transaction") != tx_dir.name or record.get("namespace") != namespace:
            raise StateError("a malformed transaction blocks another clean")
        if record.get("phase") not in {"original_parked", "restored", "aborted"}:
            raise StateError("an unresolved previous transaction blocks another clean")


def restore(transaction_id: str, support: Optional[pathlib.Path] = None, _test_crash_at: Optional[str] = None) -> dict[str, object]:
    if not re.fullmatch(r"[0-9a-f]{32}", transaction_id):
        raise StateError("transaction ID must be a 32-character local transaction ID")
    support = support or canonical_support_dir()
    control = support / CONTROL_NAME
    _validate_owned_dir(control)
    tx_dir, record_path, record = _load_record(control, transaction_id)
    namespace = str(record.get("namespace"))
    root, _ = namespace_paths(namespace, support)
    with maintenance_locks(control, namespace, exclusive=True):
        original = tx_dir / "original-root"
        _refuse_legacy_profile_owners(root, (original,))
        locked_tx_dir, locked_record_path, record = _load_record(control, transaction_id)
        if locked_tx_dir != tx_dir or locked_record_path != record_path or record.get("namespace") != namespace:
            raise StateError("transaction identity changed while waiting for maintenance ownership")
        clean_session = tx_dir / "clean-root"
        original_identity = record.get("original_identity")
        if record.get("phase") == "restored" and _identity_if_directory(root) == original_identity:
            return {"namespace": namespace, "state": "already-restored", "transaction": transaction_id}
        if _identity_if_directory(original) != original_identity:
            raise StateError("original backup identity is missing or ambiguous; retained transaction " + transaction_id)
        if os.stat(original).st_dev != os.stat(root.parent).st_dev:
            raise StateError("original backup and canonical root are not on the same filesystem")
        current = _identity_if_directory(root)
        if current is not None:
            _validate_owned_dir(root)
            if clean_session.exists():
                raise StateError("clean session backup already exists; refusing to overwrite it")
            if os.stat(root).st_dev != os.stat(original).st_dev:
                raise StateError("clean and original roots are not on the same filesystem")
            record["phase"] = "restore_prepared"
            record["clean_identity"] = current
            _write_record(record_path, record)
            _test_crash(_test_crash_at, "restore_after_prepare")
            if _identity_if_directory(root) != current or _identity_if_directory(original) != original_identity:
                raise StateError("root identity changed before restore parking; transaction retained")
            os.rename(root, clean_session)
            _fsync_dir(root.parent)
            _fsync_dir(tx_dir)
            _test_crash(_test_crash_at, "restore_after_clean_park")
            record["phase"] = "clean_parked"
            _write_record(record_path, record)
        elif record.get("phase") == "restore_prepared":
            if _identity_if_directory(clean_session) != record.get("clean_identity"):
                raise StateError("restore recovery found ambiguous clean-session identity")
            record["phase"] = "clean_parked"
            _write_record(record_path, record)
        if _identity_if_directory(original) != original_identity:
            raise StateError("original root identity changed before restore")
        os.rename(original, root)
        _fsync_dir(root.parent)
        _fsync_dir(tx_dir)
        _test_crash(_test_crash_at, "restore_after_original_rename")
        if _identity_if_directory(root) != original_identity:
            raise StateError("restored root identity did not match the retained original")
        record["phase"] = "restored"
        _write_record(record_path, record)
        return {"namespace": namespace, "state": "restored", "transaction": transaction_id, "clean_session": str(clean_session) if clean_session.exists() else None}


def recover(transaction_id: str, support: Optional[pathlib.Path] = None) -> dict[str, object]:
    support = support or canonical_support_dir()
    control = support / CONTROL_NAME
    _validate_owned_dir(support)
    _validate_owned_dir(control)
    tx_dir, record_path, namespace, record = _locate_transaction(control, transaction_id)
    root, _ = namespace_paths(namespace, support)
    with maintenance_locks(control, namespace, exclusive=True):
        locked_tx_dir, locked_record_path, locked_namespace, record = _locate_transaction(control, transaction_id)
        if locked_tx_dir != tx_dir or locked_record_path != record_path or locked_namespace != namespace:
            raise StateError("transaction identity changed while waiting for maintenance ownership")
        _refuse_legacy_profile_owners(root, (tx_dir / "original-root", tx_dir / "clean-root"))
        original = tx_dir / "original-root"
        clean_session = tx_dir / "clean-root"
        if record is None:
            if _entry_exists(original) or _entry_exists(clean_session):
                raise StateError("unrecorded transaction has a backup root; preserve it for manual recovery")
            root_identity = _identity_if_directory(root)
            if root_identity is None:
                raise StateError("unrecorded transaction has no canonical root identity; preserve it for manual recovery")
            for entry in _bounded_children(tx_dir, "unrecorded transaction recovery"):
                if entry.name == "transaction.json":
                    continue
                if not re.fullmatch(r"transaction\.json\.[0-9a-f]{32}\.tmp", entry.name):
                    raise StateError("unrecorded transaction contains unexpected evidence; preserve it for manual recovery")
                _validate_regular_owned(entry)
            record = {
                "schema": SCHEMA,
                "transaction": transaction_id,
                "namespace": namespace,
                "phase": "aborted",
                "root": str(root),
                "original_identity": root_identity,
                "recovery_classification": "aborted-before-initial-record-commit",
                "created_unix_ns": time.time_ns(),
            }
            _write_record(record_path, record)
            if _identity_if_directory(root) != root_identity:
                raise StateError("canonical root identity changed while classifying aborted preparation")
            return {"state": "aborted-before-initial-record", "transaction": transaction_id}
        orig_identity = record.get("original_identity")
        root_identity = _identity_if_directory(root)
        backup_identity = _identity_if_directory(original)
        phase = record.get("phase")
        if phase == "prepared" and root_identity == orig_identity and backup_identity is None:
            record["phase"] = "aborted"
            _write_record(record_path, record)
            return {"state": "aborted-before-rename", "transaction": transaction_id}
        if phase in {"restore_prepared", "clean_parked"} and backup_identity == orig_identity:
            clean_session = tx_dir / "clean-root"
            clean_identity = record.get("clean_identity")
            if root_identity is not None:
                if root_identity != clean_identity or _identity_if_directory(clean_session) is not None:
                    raise StateError("recovery found ambiguous clean-session identity")
                os.rename(root, clean_session)
                _fsync_dir(root.parent)
                _fsync_dir(tx_dir)
            elif _identity_if_directory(clean_session) != clean_identity:
                raise StateError("recovery found ambiguous clean-session identity")
            record["phase"] = "clean_parked"
            _write_record(record_path, record)
            os.rename(original, root)
            _fsync_dir(root.parent)
            _fsync_dir(tx_dir)
            record["phase"] = "restored"
            _write_record(record_path, record)
            return {"state": "restored", "transaction": transaction_id}
        if phase in {"prepared", "original_parked"} and root_identity is None and backup_identity == orig_identity:
            record["phase"] = "original_parked"
            _write_record(record_path, record)
            return {"state": "original-parked", "transaction": transaction_id}
        if root_identity == orig_identity and backup_identity is None:
            record["phase"] = "restored"
            _write_record(record_path, record)
            return {"state": "restored", "transaction": transaction_id}
        raise StateError("recovery found ambiguous root identities; retained transaction " + transaction_id)


def _load_record(control: pathlib.Path, transaction_id: str) -> tuple[pathlib.Path, pathlib.Path, dict[str, object]]:
    tx_dir, record_path, _namespace, record = _locate_transaction(control, transaction_id)
    if record is None:
        raise StateError("transaction has no committed record; explicit recovery is required")
    return tx_dir, record_path, record


def archive(transaction_id: str, reason: str, support: Optional[pathlib.Path] = None) -> dict[str, object]:
    """Retain an explicitly superseded reset backup outside active recovery.

    Archival never supplies restoration or volume ownership authority and never
    rewrites a legacy device identity after a remount.
    """
    if not re.fullmatch(r"[0-9a-f]{32}", transaction_id):
        raise StateError("transaction ID must be a 32-character local transaction ID")
    if not reason.strip() or len(reason) > 2048:
        raise StateError("archiving requires a nonempty reason of at most 2048 characters")
    support = support or canonical_support_dir()
    control = support / CONTROL_NAME
    _validate_owned_dir(control)
    tx_dir, record_path, namespace, record = _locate_transaction(control, transaction_id, archival=True)
    root = support / NAMESPACES[namespace]
    with maintenance_locks(control, namespace, exclusive=True):
        locked_dir, locked_record, locked_namespace, record = _locate_transaction(control, transaction_id, archival=True)
        if (locked_dir, locked_record, locked_namespace) != (tx_dir, record_path, namespace):
            raise StateError("transaction identity changed while waiting for maintenance ownership")
        if record is None or record.get("phase") != "original_parked":
            raise StateError("only an explicitly superseded parked reset backup can be archived")
        original = tx_dir / "original-root"
        original_identity = _identity_if_directory(original)
        current_identity = _identity_if_directory(root)
        if current_identity is None or original_identity is None or current_identity == original_identity:
            raise StateError("archiving requires a distinct current normal root and retained original")
        if original_identity["inode"] != record["original_identity"]["inode"]:
            raise StateError("retained original inode differs; preserve it for review")
        if current_identity["device"] != original_identity["device"]:
            raise StateError("archive and current root must remain on the same filesystem")
        _refuse_legacy_profile_owners(root, (original, tx_dir / "clean-root"))
        transaction_identity = _identity_if_directory(tx_dir)
        record_bytes = record_path.read_bytes()
        history = control / "history"
        _mkdir_owned(history)
        namespace_history = history / NAMESPACES[namespace]
        _mkdir_owned(namespace_history)
        if namespace_history.stat().st_dev != original_identity["device"]:
            raise StateError("historical evidence must remain on the same filesystem")
        slot = namespace_history / transaction_id
        # A new private slot prevents replacing an existing historical archive.
        slot.mkdir(mode=0o700)
        _fsync_dir(namespace_history)
        destination = slot / "transaction"
        receipt = {
            "schema": SCHEMA, "transaction": transaction_id, "namespace": namespace,
            "reason": reason.strip(), "archived_unix_ns": time.time_ns(),
            "source": str(tx_dir), "destination": str(destination),
            "record_sha256": hashlib.sha256(record_bytes).hexdigest(),
            "observed_transaction_identity": transaction_identity,
            "observed_original_identity": original_identity,
            "current_root_identity": current_identity,
            "restoration_authority": False,
        }
        _write_record(slot / "archive.json", receipt)
        if (_identity_if_directory(tx_dir) != transaction_identity
                or _identity_if_directory(original) != original_identity
                or _identity_if_directory(root) != current_identity
                or record_path.read_bytes() != record_bytes):
            raise StateError("state changed before archival; reserved evidence slot retained")
        os.rename(tx_dir, destination)
        _fsync_dir(tx_dir.parent)
        _fsync_dir(slot)
        if (_identity_if_directory(destination) != transaction_identity
                or _identity_if_directory(destination / "original-root") != original_identity
                or (destination / "transaction.json").read_bytes() != record_bytes
                or _identity_if_directory(root) != current_identity):
            raise StateError("archived evidence changed; preserve the historical slot for review")
        return {"state": "archived", "transaction": transaction_id, "namespace": namespace,
                "archive": str(destination), "receipt": str(slot / "archive.json"),
                "note": "Historical evidence only; original records are unchanged and no longer an active restore candidate."}


def _locate_transaction(
    control: pathlib.Path, transaction_id: str, *, archival: bool = False
) -> tuple[pathlib.Path, pathlib.Path, str, Optional[dict[str, object]]]:
    if not re.fullmatch(r"[0-9a-f]{32}", transaction_id):
        raise StateError("transaction ID must be a 32-character local transaction ID")
    _validate_owned_dir(control.parent)
    _validate_owned_dir(control)
    backups = control / "backups"
    if not backups.exists():
        raise StateError("unknown transaction ID")
    _validate_owned_dir(backups)
    matches: list[tuple[pathlib.Path, str]] = []
    for namespace_label, namespace_dir_name in NAMESPACES.items():
        namespace_dir = backups / namespace_dir_name
        if not namespace_dir.exists():
            continue
        _validate_owned_dir(namespace_dir)
        candidate = namespace_dir / transaction_id
        if not candidate.exists():
            continue
        _validate_owned_dir(candidate)
        matches.append((candidate, namespace_label))
    if len(matches) != 1:
        raise StateError("unknown or ambiguous transaction ID")
    tx_dir, namespace = matches[0]
    record_path = tx_dir / "transaction.json"
    if not _entry_exists(record_path):
        return tx_dir, record_path, namespace, None
    _validate_regular_owned(record_path)
    try:
        record = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise StateError("transaction record is unreadable; preserve it for review") from exc
    if record.get("schema") != SCHEMA or record.get("transaction") != transaction_id:
        raise StateError("transaction record schema or identity mismatch")
    namespace = record.get("namespace")
    expected_namespace = record_path.parent.parent.name
    if namespace not in NAMESPACES or NAMESPACES[namespace] != expected_namespace:
        raise StateError("transaction namespace does not match its retained path")
    expected_root = control.parent / NAMESPACES[namespace]
    if record.get("root") != str(expected_root):
        raise StateError("transaction canonical root does not match its namespace")
    original_identity = record.get("original_identity")
    if not isinstance(original_identity, dict) or not all(
        isinstance(original_identity.get(key), int) and original_identity[key] >= 0 for key in ("device", "inode")
    ):
        raise StateError("transaction original root identity is malformed")
    if not archival and (os.stat(record_path.parent).st_dev != original_identity["device"] or os.stat(expected_root.parent).st_dev != original_identity["device"]):
        raise StateError("transaction backup and canonical root are not on the original filesystem")
    return tx_dir, record_path, namespace, record


def _entry_exists(path: pathlib.Path) -> bool:
    try:
        path.lstat()
        return True
    except FileNotFoundError:
        return False


def _refuse_legacy_profile_owners(root: pathlib.Path, additional_roots: tuple[pathlib.Path, ...] = ()) -> None:
    for candidate_root in (root,) + additional_roots:
        if not candidate_root.exists():
            continue
        _validate_owned_dir(candidate_root)
        lock_paths: list[pathlib.Path] = []
        root_lock = candidate_root / "profile-owner.lock"
        if root_lock.exists() or root_lock.is_symlink():
            lock_paths.append(root_lock)
        profiles = candidate_root / "profiles"
        if profiles.exists() or profiles.is_symlink():
            _validate_owned_dir(profiles)
            for profile in _bounded_children(profiles, "profile owner scan"):
                if profile.is_symlink():
                    raise StateError("linked profile directory prevents safe reset: " + str(profile))
                if profile.is_dir():
                    _validate_owned_dir(profile)
                    profile_lock = profile / "profile-owner.lock"
                    if profile_lock.exists() or profile_lock.is_symlink():
                        lock_paths.append(profile_lock)
        volume_registry = candidate_root / "volume-ownership"
        if volume_registry.exists() or volume_registry.is_symlink():
            _validate_owned_dir(volume_registry)
            lock_paths.extend(
                path for path in _bounded_children(volume_registry, "volume owner scan")
                if path.name.startswith("volume-") and path.name.endswith(".lock")
            )
        for lock_path in set(lock_paths):
            _probe_owned_lock(lock_path)

    # Updated processes hold namespace SH admission. This process-name check catches old
    # app binaries that predate the maintenance protocol even when the root is absent.
    try:
        import subprocess

        result = subprocess.run(["ps", "-axo", "comm="], check=True, capture_output=True, text=True)
    except (OSError, subprocess.SubprocessError):
        raise StateError("cannot verify legacy Yield processes; refusing reset")
    if any(pathlib.Path(line.strip()).name in {"Yield", "yield"} for line in result.stdout.splitlines() if line.strip()):
        raise StateError("a legacy Yield process may be live; refusing reset")


def _bounded_children(directory: pathlib.Path, purpose: str) -> list[pathlib.Path]:
    children: list[pathlib.Path] = []
    with os.scandir(directory) as entries:
        for entry in entries:
            if len(children) >= 4096:
                raise StateError(f"{purpose} exceeded its safety bound; refusing reset")
            children.append(pathlib.Path(entry.path))
    return children


def _probe_owned_lock(lock_path: pathlib.Path) -> None:
    _validate_regular_owned(lock_path)
    fd = os.open(lock_path, os.O_RDWR | getattr(os, "O_NOFOLLOW", 0))
    try:
        _validate_regular_owned(lock_path, fd)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise StateError("active profile or volume owner lock prevents reset: " + str(lock_path)) from exc
        finally:
            try:
                fcntl.flock(fd, fcntl.LOCK_UN)
            except OSError:
                pass
    finally:
        os.close(fd)
def _open_lock(path: pathlib.Path) -> int:
    fd = os.open(path, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
    _validate_regular_owned(path, fd)
    _fsync_dir(path.parent)
    return fd


def _mkdir_owned(path: pathlib.Path) -> None:
    created = False
    try:
        path.mkdir(mode=0o700)
        created = True
    except FileExistsError:
        pass
    _validate_owned_dir(path)
    if created:
        _fsync_dir(path.parent)


def _validate_owned_dir(path: pathlib.Path) -> None:
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or metadata.st_uid != os.geteuid():
        raise StateError(f"path is not a real current-user-owned directory: {path}")


def _validate_regular_owned(path: pathlib.Path, fd: Optional[int] = None) -> None:
    metadata = os.fstat(fd) if fd is not None else path.lstat()
    entry = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(entry.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_nlink != 1:
        raise StateError(f"path is not a regular current-user-owned file: {path}")
    if fd is not None and (metadata.st_dev, metadata.st_ino) != (entry.st_dev, entry.st_ino):
        raise StateError(f"opened file identity changed: {path}")


def _identity_if_directory(path: pathlib.Path) -> Optional[dict[str, int]]:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode) or metadata.st_uid != os.geteuid():
        raise StateError(f"managed root is foreign, linked, or not a directory: {path}")
    return {"device": metadata.st_dev, "inode": metadata.st_ino}


def _write_record(path: pathlib.Path, record: dict[str, object], _test_crash_at: Optional[str] = None) -> None:
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
    fd = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", closefd=False) as handle:
            encoded = json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
            if _test_crash_at == "clean_during_initial_record":
                handle.write(encoded[: max(1, len(encoded) // 2)])
                handle.flush()
                os.fsync(handle.fileno())
                os._exit(87)
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
    finally:
        os.close(fd)
    os.replace(temporary, path)
    _fsync_dir(path.parent)


def _test_crash(requested: Optional[str], point: str) -> None:
    if requested == point:
        os._exit(86)


def _fsync_dir(path: pathlib.Path) -> None:
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)
