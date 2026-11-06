"""The shared local bundle packager creates sealed ad-hoc review bundles."""

from __future__ import annotations

import importlib.util
import pathlib
import platform
import plistlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
PACKAGER_PATH = ROOT / "scripts/package-gpui-macos.py"
SPEC = importlib.util.spec_from_file_location("yield_package_helper", PACKAGER_PATH)
assert SPEC is not None and SPEC.loader is not None
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)


@unittest.skipUnless(platform.system() == "Darwin", "Yield review bundles are macOS-only")
class BundlePackagerTests(unittest.TestCase):
    def test_bundle_build_targets_follow_the_exact_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            checkout = pathlib.Path(temporary)
            binary = checkout / "target/release-local/yield"
            self.assertEqual(PACKAGER.bundle_build_targets(checkout, binary), (["-p", "yield"], None))
            bin_source = checkout / "crates/yield-source-watcher/src/bin"
            for source in (
                bin_source / f"{PACKAGER.WATCHER_HELPER}.rs",
                bin_source / PACKAGER.WATCHER_HELPER / "main.rs",
            ):
                with self.subTest(source=source):
                    source.parent.mkdir(parents=True, exist_ok=True)
                    source.touch()
                    targets, helper = PACKAGER.bundle_build_targets(checkout, binary)
                    self.assertEqual(
                        targets,
                        ["-p", "yield", "-p", "yield-source-watcher", "--bin", "yield", "--bin", PACKAGER.WATCHER_HELPER],
                    )
                    self.assertEqual(helper, binary.with_name(PACKAGER.WATCHER_HELPER))
                    source.unlink()

    def test_shared_packager_signs_bundle_after_assembling_resources(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            icon = root / "Yield.icns"
            license_file = root / "LICENSE.txt"
            icon.write_bytes(b"icon fixture")
            license_file.write_text("license fixture", encoding="utf-8")
            app = root / "Yield.app"
            info = {
                "CFBundleExecutable": "yield",
                "CFBundleIdentifier": "org.portalsurfer.yield.test",
                "CFBundlePackageType": "APPL",
                "YieldChannel": "test",
            }
            signature = PACKAGER.create_signed_bundle(
                pathlib.Path(sys.executable),
                app,
                icon,
                license_file,
                info,
                cwd=ROOT,
                watcher_helper=pathlib.Path(sys.executable).resolve(),
            )
            self.assertIn("Signature=adhoc", signature)
            result = subprocess.run(
                ["codesign", "--verify", "--strict", "--deep", str(app)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                plistlib.loads((app / "Contents/Info.plist").read_bytes())["CFBundleIdentifier"],
                "org.portalsurfer.yield.test",
            )
            self.assertEqual((app / "Contents/Resources/Yield.icns").read_bytes(), icon.read_bytes())
            self.assertEqual(
                (app / "Contents/Resources/IoskeleyMono-LICENSE.txt").read_bytes(),
                license_file.read_bytes(),
            )
            helper = app / "Contents/MacOS" / PACKAGER.WATCHER_HELPER
            self.assertTrue(helper.is_file())
            subprocess.run(["codesign", "--verify", "--strict", str(helper)], check=True)

    def test_invalid_watcher_helper_refuses_before_bundle_creation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            missing = root / "missing-helper"
            nonexecutable = root / "nonexecutable"
            nonexecutable.write_text("fixture")
            symlink = root / "aliased-helper"
            symlink.symlink_to(sys.executable)
            app = root / "Yield.app"
            for invalid in (missing, nonexecutable, symlink):
                with self.subTest(invalid=invalid), self.assertRaisesRegex(
                    RuntimeError, "missing or invalid macOS watcher helper"
                ):
                    PACKAGER.create_signed_bundle(
                        pathlib.Path(sys.executable), app, missing, missing, {},
                        cwd=ROOT, watcher_helper=invalid,
                    )
                self.assertFalse(app.exists())


if __name__ == "__main__":
    unittest.main()
