"""Contract checks for the single coordinator-owned preflight checkout."""

import argparse
from contextlib import contextmanager, redirect_stdout
import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest import mock

from scripts.internal.ci.test_local_ci_contract import MODULE


class FrozenPreflightTests(unittest.TestCase):
    @contextmanager
    def fixture(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = pathlib.Path(temporary).resolve() / "repo"
            repo.mkdir()
            original_command_output = MODULE.command_output

            def git_at_fixture(*args, **kwargs):
                kwargs.setdefault("cwd", repo)
                return original_command_output(*args, **kwargs)

            with (
                mock.patch.object(MODULE, "ROOT", repo),
                mock.patch.object(MODULE, "command_output", side_effect=git_at_fixture),
            ):
                self.git("init", "--initial-branch=main")
                self.git("config", "user.email", "frozen-test@example.invalid")
                self.git("config", "user.name", "Frozen checkout test")
                (repo / "source.txt").write_text("first\n")
                (repo / ".gitignore").write_text("ignored.txt\n")
                self.git("add", ".")
                self.git("commit", "-m", "Initial")
                sha = self.git("rev-parse", "HEAD")
                yield repo, sha

    @staticmethod
    def git(*args, cwd=None):
        kwargs = {"cwd": cwd} if cwd is not None else {}
        return MODULE.command_output("git", *args, **kwargs)

    def test_reuses_the_exact_registered_path_and_checks_out_a_new_commit(self):
        with self.fixture() as (repo, first):
            caller_data = repo / "caller-untracked.txt"
            caller_data.write_text("preserve caller\n")
            with MODULE.common_coordinator() as state_dir:
                with MODULE.validation_worktree(first, state_dir, "preflight") as (path, how, warning):
                    self.assertEqual((how, warning), ("created", None))
                    lease = path.parent / "active"
                    self.assertTrue(lease.is_file())
                    original = (path / "source.txt").stat()
                    other = os.open(state_dir / "active.lock", os.O_RDWR)
                    try:
                        with self.assertRaises(BlockingIOError):
                            MODULE.fcntl.flock(other, MODULE.fcntl.LOCK_EX | MODULE.fcntl.LOCK_NB)
                    finally:
                        os.close(other)
                self.assertFalse(lease.exists())
                with MODULE.validation_worktree(first, state_dir, "preflight") as reopened:
                    self.assertEqual(reopened, (path, "reused", None))
                    self.assertEqual((path / "source.txt").stat().st_mtime_ns, original.st_mtime_ns)

                (repo / "source.txt").write_text("second\n")
                self.git("commit", "-am", "Advance")
                second = self.git("rev-parse", "HEAD")
                with MODULE.validation_worktree(second, state_dir, "preflight") as advanced:
                    self.assertEqual(advanced, (path, "updated", None))
                    self.assertEqual(self.git("rev-parse", "HEAD", cwd=path), second)
                    self.assertEqual((path / "source.txt").read_text(), "second\n")
                self.assertEqual(caller_data.read_text(), "preserve caller\n")
                self.assertEqual(self.git("branch", "--show-current"), "main")

    def test_dirty_ignored_file_and_hidden_index_are_left_in_place(self):
        with self.fixture() as (_, sha), MODULE.common_coordinator() as state_dir:
            with MODULE.validation_worktree(sha, state_dir, "preflight") as (cached, _, _):
                pass
            extra = cached / "ignored.txt"
            extra.write_text("do not remove\n")
            with MODULE.validation_worktree(sha, state_dir, "preflight") as (temporary, how, reason):
                self.assertNotEqual(temporary, cached)
                self.assertEqual(how, "temporary")
                self.assertIn("extra files", reason)
            self.assertFalse(temporary.exists())
            self.assertEqual(extra.read_text(), "do not remove\n")
            extra.unlink()

            self.git("update-index", "--assume-unchanged", "source.txt", cwd=cached)
            (cached / "source.txt").write_text("hidden change\n")
            with MODULE.validation_worktree(sha, state_dir, "preflight") as (_, how, reason):
                self.assertEqual(how, "temporary")
                self.assertIn("hidden", reason)
            self.assertEqual((cached / "source.txt").read_text(), "hidden change\n")

    def test_unconfirmed_cleanup_preserves_the_lease_and_forces_a_fresh_checkout(self):
        with self.fixture() as (_, sha), MODULE.common_coordinator() as state_dir:
            with self.assertRaisesRegex(MODULE.GroupCleanupUnconfirmed, "fixture"):
                with MODULE.validation_worktree(sha, state_dir, "preflight") as (cached, _, _):
                    raise MODULE.GroupCleanupUnconfirmed("fixture process still active")
            lease = cached.parent / "active"
            self.assertTrue(lease.is_file())
            with MODULE.validation_worktree(sha, state_dir, "preflight") as (temporary, how, reason):
                self.assertNotEqual(temporary, cached)
                self.assertEqual(how, "temporary")
                self.assertIn("unconfirmed", reason)
            self.assertTrue(lease.is_file())

    def test_foreign_slot_is_preserved_without_opening_its_marker(self):
        with self.fixture() as (repo, sha), MODULE.common_coordinator() as state_dir:
            slot = state_dir / MODULE.FROZEN_PREFLIGHT_SLOT
            slot.mkdir(mode=0o700)
            marker = slot / "owner.json"
            marker.symlink_to(repo / "source.txt")
            with MODULE.validation_worktree(sha, state_dir, "preflight") as (_, how, reason):
                self.assertEqual(how, "temporary")
                self.assertIn("ownership marker", reason)
            self.assertTrue(marker.is_symlink())
            self.assertEqual((repo / "source.txt").read_text(), "first\n")
            self.assertFalse((slot / "checkout").exists())

    def test_preflight_executes_each_run_and_records_failure_on_the_shared_checkout(self):
        with self.fixture() as (_, sha):
            results = []
            args = argparse.Namespace(sha=sha, lane="preflight", case=None)
            with (
                mock.patch.object(MODULE, "doctor", return_value={}),
                mock.patch.object(MODULE, "cargo_env", return_value={}),
                mock.patch.object(MODULE, "run_managed", side_effect=[7, 0]) as runner,
            ):
                for exit_status in (7, 0):
                    output = io.StringIO()
                    with redirect_stdout(output):
                        if exit_status:
                            with self.assertRaisesRegex(MODULE.CiError, "preflight lane failed"):
                                MODULE._check(args)
                        else:
                            MODULE._check(args)
                    results.append(json.loads(output.getvalue()))
                self.assertEqual(runner.call_count, 2)
                self.assertEqual(runner.call_args_list[0].kwargs["cwd"], runner.call_args_list[1].kwargs["cwd"])
                self.assertEqual(runner.call_args_list[1].kwargs["env"]["PYTHONDONTWRITEBYTECODE"], "1")
            self.assertEqual([r["status"] for r in results], ["failed", "passed"])
            self.assertEqual([r["checkout"]["disposition"] for r in results], ["created", "reused"])
            self.assertNotEqual(results[0]["run_id"], results[1]["run_id"])
            self.assertEqual([r["results"][0]["exit_code"] for r in results], [7, 0])


if __name__ == "__main__":
    unittest.main()
