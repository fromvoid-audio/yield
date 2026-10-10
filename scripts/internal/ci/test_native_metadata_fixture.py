from __future__ import annotations

import pathlib
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import prepare_native_metadata_fixture as fixture
from prepare_native_metadata_fixture import FixtureError, prepare


class NativeMetadataFixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.base = pathlib.Path(self.temp.name)
        self.checkout = self.base / "checkout"
        self.source = self.checkout / "crates/yield-catalog/tests/fixtures/native-metadata"
        (self.source / "Library").mkdir(parents=True)
        (self.source / "Library/.DS_Store").write_bytes(b"metadata fixture\n")
        (self.source / "stable.wav").write_bytes(b"stable audio fixture\n")
        for path in (self.source / "Library/.DS_Store", self.source / "stable.wav"):
            path.chmod(0o644)
        self.state_dir = self.base / "common.git/local-ci"
        self.state_dir.mkdir(parents=True, mode=0o700)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_reverification_preserves_keyed_root_and_file_identities(self) -> None:
        first = prepare(self.state_dir, self.checkout)
        source_root = pathlib.Path(first["source_root"])
        before = (source_root.stat().st_ino, (source_root / "stable.wav").stat().st_ino)
        second = prepare(self.state_dir, self.checkout)
        after = (source_root.stat().st_ino, (source_root / "stable.wav").stat().st_ino)
        self.assertEqual(first, second)
        self.assertEqual(before, after)

    def test_existing_content_mismatch_is_refused_without_repair(self) -> None:
        first = prepare(self.state_dir, self.checkout)
        audio = pathlib.Path(first["source_root"]) / "stable.wav"
        audio.write_bytes(b"tampered\n")
        with self.assertRaisesRegex(FixtureError, "content differs"):
            prepare(self.state_dir, self.checkout)
        self.assertEqual(audio.read_bytes(), b"tampered\n")

    def test_replaced_file_with_same_bytes_is_refused_by_stored_inode_identity(self) -> None:
        first = prepare(self.state_dir, self.checkout)
        audio = pathlib.Path(first["source_root"]) / "stable.wav"
        original_bytes = audio.read_bytes()
        original_identity = (audio.stat().st_dev, audio.stat().st_ino)
        with audio.open("rb") as retained_old_inode:
            audio.unlink()
            audio.write_bytes(original_bytes)
            audio.chmod(0o600)
            replacement_identity = (audio.stat().st_dev, audio.stat().st_ino)
            self.assertNotEqual(original_identity, replacement_identity)
            with self.assertRaisesRegex(FixtureError, "identity or manifest differs"):
                prepare(self.state_dir, self.checkout)
            self.assertEqual(audio.read_bytes(), original_bytes)
            self.assertEqual(retained_old_inode.read(), original_bytes)

    def test_symlinked_fixture_file_is_refused(self) -> None:
        first = prepare(self.state_dir, self.checkout)
        audio = pathlib.Path(first["source_root"]) / "stable.wav"
        target = audio.with_name("outside.wav")
        audio.rename(target)
        audio.symlink_to(target.name)
        with self.assertRaises(FixtureError):
            prepare(self.state_dir, self.checkout)
        self.assertTrue(target.exists())

    def test_symlinked_intermediate_source_directory_is_refused(self) -> None:
        prepare(self.state_dir, self.checkout)
        library = self.source / "Library"
        outside = self.checkout / "retained-library"
        library.rename(outside)
        library.symlink_to(outside, target_is_directory=True)
        with self.assertRaises(FixtureError):
            prepare(self.state_dir, self.checkout)
        self.assertEqual((outside / ".DS_Store").read_bytes(), b"metadata fixture\n")

    def test_unknown_namespace_entry_is_refused_and_retained(self) -> None:
        prepare(self.state_dir, self.checkout)
        namespace = self.state_dir / "fixtures/native-metadata"
        unknown = namespace / "unexpected"
        unknown.write_text("retained evidence", encoding="utf-8")
        with self.assertRaisesRegex(FixtureError, "unrecognized native metadata fixture entries"):
            prepare(self.state_dir, self.checkout)
        self.assertEqual(unknown.read_text(encoding="utf-8"), "retained evidence")

    def test_destination_created_at_publish_is_never_overwritten(self) -> None:
        original_rename = fixture._rename_noreplace
        observed: dict[str, object] = {}

        def race_create_destination(src_fd: int, src_name: str, dst_fd: int, dst_name: str) -> None:
            os.mkdir(dst_name, mode=0o700, dir_fd=dst_fd)
            target_fd = os.open(dst_name, os.O_RDONLY | os.O_DIRECTORY, dir_fd=dst_fd)
            try:
                observed["identity"] = (os.fstat(target_fd).st_dev, os.fstat(target_fd).st_ino)
                marker = os.open("unrecognized", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=target_fd)
                os.write(marker, b"retain collision evidence")
                os.close(marker)
            finally:
                os.close(target_fd)
            original_rename(src_fd, src_name, dst_fd, dst_name)

        with mock.patch.object(fixture, "_rename_noreplace", side_effect=race_create_destination):
            with self.assertRaisesRegex(FixtureError, "destination appeared"):
                prepare(self.state_dir, self.checkout)

        namespace = self.state_dir / "fixtures/native-metadata"
        key_dirs = [path for path in namespace.iterdir() if len(path.name) == 64]
        self.assertEqual(len(key_dirs), 1)
        target = key_dirs[0]
        self.assertEqual((target.stat().st_dev, target.stat().st_ino), observed["identity"])
        self.assertEqual((target / "unrecognized").read_bytes(), b"retain collision evidence")
        staging = list(namespace.glob(f".{target.name}.prepare-*"))
        self.assertEqual(len(staging), 1)
        self.assertTrue((staging[0] / "manifest.json").is_file())
        self.assertTrue((staging[0] / "source/Library/.DS_Store").is_file())


if __name__ == "__main__":
    unittest.main()
