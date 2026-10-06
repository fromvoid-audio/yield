"""Exercise the real public Markdown checker in disposable Git repositories."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[3]


class MarkdownLinks(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="yield markdown links ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in (
            "scripts/check.sh",
            "scripts/internal/check/check_markdown_links.sh",
            "scripts/internal/check/check_markdown_links.py",
            "scripts/internal/check/check_markdown_links.ps1",
        ):
            source = SOURCE / name
            if source.exists():
                target = self.root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
        self.git("init", "--quiet")
        self.git("config", "user.name", "Markdown fixture")
        self.git("config", "user.email", "markdown-fixture@example.invalid")
        self.write("README.md", "# Existing target\n")
        self.write("docs/note.md", "# Note\n")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(
            ["git", *args], cwd=self.root, text=True, stderr=subprocess.STDOUT
        )

    def write(self, name, content):
        target = self.root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")

    def commit(self):
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "Markdown fixture")

    def check(self, *args):
        return subprocess.run(
            ["bash", "scripts/check.sh", "markdown-links", *args],
            cwd=self.root,
            text=True,
            capture_output=True,
            timeout=20,
        )

    def assert_broken(self, result, file, target):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(file, result.stderr)
        self.assertIn(target, result.stderr)

    def test_committed_broken_link(self):
        self.write("docs/note.md", "[Broken](missing.md)\n")
        self.commit()
        self.assert_broken(
            self.check("--base", self.base, "--head", "HEAD"),
            "docs/note.md",
            "missing.md",
        )

    def test_committed_broken_image(self):
        self.write("docs/note.md", "![Broken image](missing.png#caption)\n")
        self.commit()
        self.assert_broken(self.check(), "docs/note.md", "missing.png#caption")

    def test_staged_change(self):
        self.write("docs/note.md", "[Staged](missing-staged.md)\n")
        self.git("add", "docs/note.md")
        self.assert_broken(
            self.check("--base", self.base), "docs/note.md", "missing-staged.md"
        )

    def test_unstaged_change(self):
        self.write("docs/note.md", "[Unstaged](missing-unstaged.md)\n")
        self.assert_broken(
            self.check("--base", self.base), "docs/note.md", "missing-unstaged.md"
        )

    def test_spaced_document_and_target(self):
        self.write("docs/spaced target.md", "# Existing target\n")
        self.write("docs/spaced note.md", "[Existing](<spaced target.md>)\n")
        self.commit()
        result = self.check("--base", self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.write("docs/spaced note.md", "[Missing](<missing target.md>)\n")
        self.assert_broken(
            self.check("--base", self.base),
            "docs/spaced note.md",
            "<missing target.md>",
        )

    def test_existing_targets_fragments_queries_and_exclusions(self):
        self.write("docs/image.png", "image fixture\n")
        self.write(
            "docs/note.md",
            "[Existing](../README.md#heading)\n"
            "![Existing image](image.png?download=1#caption)\n"
            "[HTTPS](https://example.invalid/missing)\n"
            "[HTTP](http://example.invalid/missing)\n"
            "[Mail](mailto:test@example.invalid)\n"
            "[Phone](tel:123)\n"
            "[Site](/missing-site-path)\n"
            "[Anchor](#missing-heading)\n"
            "[Template](${MISSING}/file.md)\n"
            "[Other template]({{missing}}/file.md)\n",
        )
        self.commit()
        result = self.check("--base", self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_explicit_head_selects_committed_range(self):
        self.write("docs/note.md", "[Broken](missing.md)\n")
        self.commit()
        broken = self.git("rev-parse", "HEAD").strip()
        self.write("docs/note.md", "[Existing](../README.md)\n")
        self.commit()
        # Selection follows the requested range; contents are the current worktree.
        good = self.check("--base", self.base, "--head", broken)
        self.assertEqual(good.returncode, 0, good.stdout + good.stderr)
        self.write("docs/note.md", "[Current broken](current-missing.md)\n")
        self.assert_broken(
            self.check("--base", self.base, "--head", broken),
            "docs/note.md",
            "current-missing.md",
        )

    def test_non_markdown_change(self):
        self.write("data.txt", "Unrelated change\n")
        self.commit()
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("No changed Markdown files detected", result.stdout)

    def test_unknown_cli_argument(self):
        result = self.check("--not-a-real-option")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("Unknown argument", result.stderr)


if __name__ == "__main__":
    unittest.main()
