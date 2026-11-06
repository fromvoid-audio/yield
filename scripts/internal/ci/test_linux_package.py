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


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


PACKAGER = load_script("package-gpui-linux")
DESKTOP = load_script("register-linux-desktop")


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
                self.assertIn("yield/scripts/register-linux-desktop.py", archive.namelist())
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

    def test_desktop_entry_targets_one_package_and_preserves_a_collision(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            package = root / 'Yield test "$assets"'
            icon = package / "assets" / "yield-brand-icon.png"
            icon.parent.mkdir(parents=True)
            icon.write_bytes(b"fixture icon")
            binary = package / "yield"
            binary.write_bytes(b"fixture executable")
            binary.chmod(0o700)
            data_home = root / "desktop data"
            destination = DESKTOP.register_desktop(package, data_home)
            self.assertEqual(
                destination, data_home / "applications" / "org.portalsurfer.yield.desktop"
            )
            content = destination.read_text(encoding="utf-8")
            fields = dict(line.split("=", 1) for line in content.splitlines()[1:])
            expected_exec = f'"{root}/' + r'Yield test \\"\\$assets\\"/yield"'
            self.assertEqual(fields["Exec"], expected_exec)
            self.assertEqual(fields["Icon"], str(icon))
            self.assertEqual(fields["StartupWMClass"], "org.portalsurfer.yield")
            self.assertEqual(fields["Categories"], "AudioVideo;Audio;")
            self.assertNotIn("MimeType", fields)

            destination.write_text("existing user registration\n", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                DESKTOP.register_desktop(package, data_home)
            self.assertEqual(destination.read_text(encoding="utf-8"), "existing user registration\n")
            self.assertEqual(list(destination.parent.iterdir()), [destination])
            with self.assertRaisesRegex(ValueError, "choose an ASCII install path"):
                DESKTOP.quoted_exec(root / "path-%f" / "yield")


if __name__ == "__main__":
    unittest.main()
