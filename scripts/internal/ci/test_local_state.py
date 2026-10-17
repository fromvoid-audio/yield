from __future__ import annotations

import json
import fcntl
import os
import pathlib
import sys
import subprocess
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from local_state import (
    StateError,
    archive,
    clean,
    convert_clean_handoff_to_validator_lease,
    maintenance_locks,
    namespace_paths,
    prepare_clean_launch,
    recover,
    restore,
    status,
)


class LocalStateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.support = pathlib.Path(self.temp.name)
        self.root, self.control = namespace_paths("development", self.support)
        self.root.mkdir()
        (self.root / "dirty.marker").write_text("retained evidence", encoding="utf-8")
        self.original_identity = (self.root.stat().st_dev, self.root.stat().st_ino)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_normal_state_status_does_not_replace_root(self) -> None:
        before = self.root.stat().st_ino
        self.assertEqual(status("development", self.support)["state"], "present")
        (self.root / "second-launch").write_text("same state", encoding="utf-8")
        self.assertEqual(self.root.stat().st_ino, before)

    def test_clean_and_restore_preserve_original_and_displaced_clean_session(self) -> None:
        result = clean("development", self.support)
        transaction = str(result["transaction"])
        self.assertFalse(self.root.exists())
        self.root.mkdir()
        (self.root / "fresh.marker").write_text("clean session", encoding="utf-8")
        restored = restore(transaction, self.support)
        self.assertEqual(restored["state"], "restored")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)
        self.assertEqual((self.root / "dirty.marker").read_text(encoding="utf-8"), "retained evidence")
        clean_session = pathlib.Path(str(restored["clean_session"]))
        self.assertEqual((clean_session / "fresh.marker").read_text(encoding="utf-8"), "clean session")

    def test_clean_refuses_live_shared_owner(self) -> None:
        with maintenance_locks(self.control, "development", exclusive=False):
            with self.assertRaises(StateError):
                clean("development", self.support)
        self.assertEqual((self.root / "dirty.marker").read_text(encoding="utf-8"), "retained evidence")

    def test_explicit_archive_preserves_remounted_backup_without_restoration_authority(self) -> None:
        transaction = str(clean("development", self.support)["transaction"])
        tx_dir = self.control / "backups" / ".yield-development" / transaction
        record_path = tx_dir / "transaction.json"
        record = json.loads(record_path.read_text())
        record["original_identity"]["device"] += 100
        record_path.write_text(json.dumps(record))
        original_record = record_path.read_bytes()
        original_marker = tx_dir / "original-root" / "dirty.marker"
        original_marker_identity = original_marker.stat().st_ino
        self.root.mkdir()
        (self.root / "current").write_text("current profile")
        current_identity = self.root.stat().st_ino
        with self.assertRaisesRegex(StateError, "original filesystem"):
            restore(transaction, self.support)
        result = archive(transaction, "Owner chose historical evidence", self.support)
        destination = pathlib.Path(result["archive"])
        self.assertFalse(tx_dir.exists())
        self.assertEqual((destination / "transaction.json").read_bytes(), original_record)
        marker = destination / "original-root" / "dirty.marker"
        self.assertEqual(marker.stat().st_ino, original_marker_identity)
        self.assertEqual(marker.read_text(), "retained evidence")
        self.assertEqual(self.root.stat().st_ino, current_identity)
        self.assertEqual((self.root / "current").read_text(), "current profile")
        self.assertFalse(json.loads(pathlib.Path(result["receipt"]).read_text())["restoration_authority"])
        with self.assertRaisesRegex(StateError, "unknown or ambiguous"):
            restore(transaction, self.support)
        next_transaction = str(clean("development", self.support)["transaction"])
        restore(next_transaction, self.support)
        self.assertEqual(self.root.stat().st_ino, current_identity)

    def test_archive_refuses_missing_current_root_live_owner_and_ambiguous_original(self) -> None:
        transaction = str(clean("development", self.support)["transaction"])
        with self.assertRaisesRegex(StateError, "distinct current normal root"):
            archive(transaction, "Historical", self.support)
        self.root.mkdir()
        with maintenance_locks(self.control, "development", exclusive=False):
            with self.assertRaisesRegex(StateError, "live owner"):
                archive(transaction, "Historical", self.support)
        tx_dir = self.control / "backups" / ".yield-development" / transaction
        record_path = tx_dir / "transaction.json"
        record = json.loads(record_path.read_text())
        record["original_identity"]["inode"] += 1
        record_path.write_text(json.dumps(record))
        with self.assertRaisesRegex(StateError, "inode differs"):
            archive(transaction, "Historical", self.support)
        self.assertTrue((tx_dir / "original-root" / "dirty.marker").exists())
        self.assertFalse((self.control / "history").exists())

    def test_validator_lease_allows_successive_admitting_children_and_blocks_reset(self) -> None:
        result, namespace_fd, transition_fd = prepare_clean_launch("development", self.support)
        try:
            lease_fd = convert_clean_handoff_to_validator_lease(
                "development", namespace_fd, transition_fd, self.support
            )
            child_source = (
                "import pathlib,sys; "
                "from local_state import maintenance_locks; "
                "control=pathlib.Path(sys.argv[1]); "
                "ctx=maintenance_locks(control,'development',False); "
                "locks=ctx.__enter__(); ctx.__exit__(None,None,None); "
                "assert len(locks)==2"
            )
            child_env = os.environ.copy()
            child_env["PYTHONPATH"] = str(pathlib.Path(__file__).resolve().parent)
            for _ in range(2):
                child = subprocess.run(
                    [sys.executable, "-c", child_source, str(self.control)],
                    env=child_env,
                    pass_fds=(lease_fd,),
                    check=False,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(child.returncode, 0, child.stderr)
            with self.assertRaises(StateError):
                clean("development", self.support)
        finally:
            os.close(namespace_fd)
            os.close(transition_fd)
            if result.get("transaction"):
                restore(str(result["transaction"]), self.support)

    def test_recover_after_root_rename_before_phase_record(self) -> None:
        completed = self._crash_child("clean", "clean_after_rename")
        self.assertEqual(completed.returncode, 86)
        tx_dir = next((self.control / "backups" / ".yield-development").iterdir())
        transaction = tx_dir.name
        record_path = tx_dir / "transaction.json"
        record_path.with_suffix(".json.tmp").write_text("partial crash record", encoding="utf-8")
        record = json.loads(record_path.read_text(encoding="utf-8"))
        outcome = recover(transaction, self.support)
        self.assertEqual(outcome["state"], "original-parked")
        self.assertFalse(self.root.exists())
        self.assertTrue((record_path.parent / "original-root" / "dirty.marker").exists())

    def test_initial_partial_record_crash_can_be_explicitly_recovered(self) -> None:
        completed = self._crash_child("clean", "clean_during_initial_record")
        self.assertEqual(completed.returncode, 87)
        backup_root = self.control / "backups" / ".yield-development"
        tx_dir = next(backup_root.iterdir())
        transaction = tx_dir.name
        partial_records = list(tx_dir.glob("transaction.json.*.tmp"))
        self.assertEqual(len(partial_records), 1)
        partial_bytes = partial_records[0].read_bytes()
        self.assertTrue(partial_bytes)
        self.assertFalse((tx_dir / "transaction.json").exists())
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)
        with self.assertRaisesRegex(StateError, "explicit recovery"):
            clean("development", self.support)

        recovered = recover(transaction, self.support)
        self.assertEqual(recovered["state"], "aborted-before-initial-record")
        record = json.loads((tx_dir / "transaction.json").read_text(encoding="utf-8"))
        self.assertEqual(record["phase"], "aborted")
        self.assertEqual(record["recovery_classification"], "aborted-before-initial-record-commit")
        self.assertEqual(partial_records[0].read_bytes(), partial_bytes)
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)

        next_clean = clean("development", self.support)
        self.assertEqual(next_clean["state"], "clean")
        self.assertEqual(restore(str(next_clean["transaction"]), self.support)["state"], "restored")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)

    def test_symlinked_managed_root_is_refused(self) -> None:
        self.root.rename(self.support / "real-root")
        self.root.symlink_to(self.support / "real-root", target_is_directory=True)
        with self.assertRaises(StateError):
            clean("development", self.support)
        self.assertTrue((self.support / "real-root" / "dirty.marker").exists())

    def test_already_vanilla_does_not_create_backup(self) -> None:
        self.root.rename(self.support / "parked-external")
        result = clean("development", self.support)
        self.assertEqual(result["state"], "already-vanilla")
        self.assertIsNone(result["transaction"])
        self.assertFalse((self.control / "backups").exists())

    def test_live_root_profile_and_volume_locks_refuse_clean(self) -> None:
        for relative in ("profile-owner.lock", "volume-ownership/volume-test.lock"):
            lock_path = self.root / relative
            lock_path.parent.mkdir(parents=True, exist_ok=True)
            lock_path.touch()
            fd = os.open(lock_path, os.O_RDWR)
            try:
                fcntl.flock(fd, fcntl.LOCK_SH)
                with self.assertRaises(StateError):
                    clean("development", self.support)
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
                os.close(fd)

    def test_recovery_completes_interrupted_restore(self) -> None:
        result = clean("development", self.support)
        transaction = str(result["transaction"])
        tx_dir = next((self.control / "backups").glob(f"*/{transaction}"))
        self.root.mkdir()
        (self.root / "fresh.marker").write_text("clean session", encoding="utf-8")
        record_path = tx_dir / "transaction.json"
        record = json.loads(record_path.read_text(encoding="utf-8"))
        clean_identity = {"device": self.root.stat().st_dev, "inode": self.root.stat().st_ino}
        record["phase"] = "restore_prepared"
        record["clean_identity"] = clean_identity
        record_path.write_text(json.dumps(record), encoding="utf-8")
        self.root.rename(tx_dir / "clean-root")
        outcome = recover(transaction, self.support)
        self.assertEqual(outcome["state"], "restored")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)
        self.assertTrue((tx_dir / "clean-root" / "fresh.marker").exists())

    def test_backup_namespace_symlink_is_refused(self) -> None:
        result = clean("development", self.support)
        transaction = str(result["transaction"])
        backups = self.control / "backups"
        namespace_dir = backups / ".yield-development"
        moved = self.support / "retained-backups"
        namespace_dir.rename(moved)
        namespace_dir.symlink_to(moved, target_is_directory=True)
        with self.assertRaises(StateError):
            restore(transaction, self.support)

    def test_unresolved_restore_preparation_blocks_new_clean(self) -> None:
        result = clean("development", self.support)
        tx_dir = next((self.control / "backups").glob(f"*/{result['transaction']}"))
        self.root.mkdir()
        record_path = tx_dir / "transaction.json"
        record = json.loads(record_path.read_text(encoding="utf-8"))
        record["phase"] = "restore_prepared"
        record["clean_identity"] = {"device": self.root.stat().st_dev, "inode": self.root.stat().st_ino}
        record_path.write_text(json.dumps(record), encoding="utf-8")
        with self.assertRaises(StateError):
            clean("development", self.support)

    def test_process_crash_at_restore_boundaries_recovers_both_sessions(self) -> None:
        original = clean("development", self.support)
        transaction = str(original["transaction"])
        self.root.mkdir()
        (self.root / "fresh.marker").write_text("fresh", encoding="utf-8")
        completed = self._crash_child("restore", "restore_after_clean_park", transaction)
        self.assertEqual(completed.returncode, 86)
        restored = recover(transaction, self.support)
        self.assertEqual(restored["state"], "restored")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)
        self.assertEqual((next((self.control / "backups").glob(f"*/{transaction}/clean-root")) / "fresh.marker").read_text(encoding="utf-8"), "fresh")

    def test_process_crash_before_clean_rename_aborts_without_moving_root(self) -> None:
        completed = self._crash_child("clean", "clean_after_prepare")
        self.assertEqual(completed.returncode, 86)
        tx_dir = next((self.control / "backups" / ".yield-development").iterdir())
        result = recover(tx_dir.name, self.support)
        self.assertEqual(result["state"], "aborted-before-rename")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)

    def test_process_crash_after_original_restore_rename_is_idempotently_recovered(self) -> None:
        original = clean("development", self.support)
        transaction = str(original["transaction"])
        self.root.mkdir()
        (self.root / "fresh.marker").write_text("fresh", encoding="utf-8")
        completed = self._crash_child("restore", "restore_after_original_rename", transaction)
        self.assertEqual(completed.returncode, 86)
        result = recover(transaction, self.support)
        self.assertEqual(result["state"], "restored")
        self.assertEqual((self.root.stat().st_dev, self.root.stat().st_ino), self.original_identity)
        self.assertTrue((next((self.control / "backups").glob(f"*/{transaction}/clean-root")) / "fresh.marker").exists())

    def _crash_child(self, operation: str, point: str, transaction: str = "") -> subprocess.CompletedProcess[str]:
        module_dir = pathlib.Path(__file__).resolve().parent
        code = (
            "import pathlib,sys; sys.path.insert(0,sys.argv[1]); import local_state; "
            "op=sys.argv[4]; point=sys.argv[5]; tx=sys.argv[6]; base=pathlib.Path(sys.argv[2]); "
            "local_state.clean('development',base,_test_crash_at=point) if op=='clean' else "
            "local_state.restore(tx,base,_test_crash_at=point)"
        )
        return subprocess.run(
            [sys.executable, "-c", code, str(module_dir), str(self.support), "unused", operation, point, transaction],
            capture_output=True,
            text=True,
            check=False,
        )


if __name__ == "__main__":
    unittest.main()
