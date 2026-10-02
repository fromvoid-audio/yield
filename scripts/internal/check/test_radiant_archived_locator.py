"""Disposable, offline checks of the retained sibling helper; no active dependency."""

import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
HELPER = ROOT / "scripts/internal/radiant/sibling.sh"


class ArchivedLocatorTests(unittest.TestCase):
    def test_missing_active_metadata_is_explicit(self):
        result = subprocess.run(["bash", str(HELPER), "locate", "--root", str(ROOT)],
                                capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing radiant-dependency.toml", result.stderr)

    def test_locator_preserves_dirty_sibling_and_clean_provision_refuses_it(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            checkout = workspace / "yield"
            sibling = workspace / "radiant"
            checkout.mkdir()
            sibling.mkdir()
            (checkout / "Cargo.toml").write_text("[workspace]\n")
            (sibling / "Cargo.toml").write_text('[package]\nname = "radiant"\nversion = "0.0.0"\n')

            def git(*args):
                return subprocess.check_output(["git", "-C", str(sibling), *args],
                                               text=True, stderr=subprocess.PIPE, timeout=10).strip()

            git("init", "--quiet")
            git("config", "user.name", "Disposable fixture")
            git("config", "user.email", "fixture@example.invalid")
            git("add", "Cargo.toml")
            git("commit", "--quiet", "-m", "fixture")
            git("remote", "add", "origin", "https://github.com/PORTALSURFER/radiant.git")
            head = git("rev-parse", "HEAD")
            (checkout / "radiant-dependency.toml").write_text(
                f'repository = "https://github.com/PORTALSURFER/radiant.git"\nrevision = "{head}"\npath = "../radiant"\n'
            )

            def helper(*args):
                return subprocess.run(["bash", str(HELPER), *args, "--root", str(checkout)],
                                      capture_output=True, text=True, timeout=10)

            clean = helper("locate")
            self.assertEqual(clean.returncode, 0, clean.stderr)
            self.assertIn("RADIANT_REVISION_MATCH=yes", clean.stdout)
            self.assertIn("RADIANT_STATE=clean", clean.stdout)
            untracked = sibling / "user-work.txt"
            untracked.write_text("preserve this fixture work\n")
            dirty = helper("locate")
            self.assertEqual(dirty.returncode, 0, dirty.stderr)
            self.assertIn("RADIANT_STATE=dirty", dirty.stdout)
            refused = helper("provision", "--clean")
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn("refusing to mutate dirty", refused.stderr)
            wrong_path = helper("locate", "--path", str(workspace / "elsewhere"))
            self.assertNotEqual(wrong_path.returncode, 0)
            self.assertIn("does not match", wrong_path.stderr)
            self.assertEqual(git("rev-parse", "HEAD"), head)
            self.assertEqual(untracked.read_text(), "preserve this fixture work\n")


if __name__ == "__main__":
    unittest.main()
