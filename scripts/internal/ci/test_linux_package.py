"""Verify the private Linux archive layout without a native build."""

import hashlib
import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("yield_linux_package", ROOT / "scripts/package-gpui-linux.py")
assert SPEC is not None and SPEC.loader is not None
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)


class LinuxPackageTest(unittest.TestCase):
    def test_archive_preserves_identity_payload_and_executable_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "yield"
            header = bytearray(64)
            header[:7] = b"\x7fELF\x02\x01\x01"
            header[16:20] = b"\x03\x00\x3e\x00"
            binary.write_bytes(header + b"synthetic binary")
            binary.chmod(0o700)
            path = root / PACKAGER.ARCHIVE_NAME
            sha = "a" * 40
            PACKAGER.assemble_archive(ROOT, binary, path, sha, "0.19.1")

            corrupt = root / "corrupt.zip"
            with zipfile.ZipFile(path) as archive, zipfile.ZipFile(corrupt, "w") as altered:
                manifest = json.loads(archive.read("yield/package-manifest.json"))
                self.assertEqual(manifest["source_sha"], sha)
                self.assertEqual(manifest["version"], "0.19.1")
                self.assertEqual(manifest["target"], "x86_64-unknown-linux-gnu")
                self.assertEqual(manifest["build_profile"], "release-local")
                self.assertEqual(manifest["installation"], "manual")
                self.assertEqual(
                    manifest["files"]["yield"]["sha256"],
                    hashlib.sha256(binary.read_bytes()).hexdigest(),
                )
                for name in ("yield/yield", "yield/scripts/doctor.sh", "yield/scripts/run.sh"):
                    self.assertEqual(archive.getinfo(name).external_attr >> 16, stat.S_IFREG | 0o755)
                self.assertIn("yield/scripts/internal/run/linux_diagnostics_paths.sh", archive.namelist())
                self.assertIn("yield/assets/yield-brand-icon.png", archive.namelist())
                for entry in archive.infolist():
                    contents = b"replacement" if entry.filename == "yield/yield" else archive.read(entry)
                    altered.writestr(entry, contents)

            with self.assertRaisesRegex(ValueError, "digest mismatch for yield"):
                PACKAGER.verify_archive(corrupt, manifest)

            header[18:20] = b"\xb7\x00"
            binary.write_bytes(header)
            rejected = root / "foreign.zip"
            with self.assertRaisesRegex(ValueError, "expected an x86_64 ELF executable"):
                PACKAGER.assemble_archive(ROOT, binary, rejected, sha, "0.19.1")
            self.assertFalse(rejected.exists())


if __name__ == "__main__":
    unittest.main()
