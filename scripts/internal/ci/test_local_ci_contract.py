"""Safety checks for the managed binary compatibility probe."""

from __future__ import annotations

import importlib.util
from importlib.machinery import SourceFileLoader
import json
import os
import pathlib
import signal
import sys
import tempfile
import unittest
from contextlib import contextmanager
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[3]
CLI = ROOT / "scripts/yield-local-ci"
SPEC = importlib.util.spec_from_loader("yield_local_ci", SourceFileLoader("yield_local_ci", str(CLI)))
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ExactWorktreeBranchTests(unittest.TestCase):
    def git(self, repo: pathlib.Path, *args: str) -> str:
        result = MODULE.subprocess.run(
            ["git", *args], cwd=repo, text=True, capture_output=True, check=True
        )
        return result.stdout.strip()

    def test_exact_feature_worktree_satisfies_guard_and_cleans_up_without_touching_caller(self) -> None:
        for body_raises in (False, True):
            with self.subTest(body_raises=body_raises), tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                repo = root / "repo"
                remote = root / "origin.git"
                repo.mkdir()
                self.git(root, "init", "--initial-branch=main", str(repo))
                self.git(repo, "config", "user.email", "ci-test@example.invalid")
                self.git(repo, "config", "user.name", "CI test")
                guard = repo / "scripts/internal/check/check_main_branch.sh"
                guard.parent.mkdir(parents=True)
                guard.write_bytes((ROOT / "scripts/internal/check/check_main_branch.sh").read_bytes())
                (repo / "tracked.txt").write_text("pinned source\n", encoding="utf-8")
                self.git(repo, "add", "tracked.txt", "scripts/internal/check/check_main_branch.sh")
                self.git(repo, "commit", "-m", "fixture")
                sha = self.git(repo, "rev-parse", "HEAD")
                self.git(root, "init", "--bare", str(remote))
                self.git(repo, "remote", "add", "origin", str(remote))
                self.git(repo, "push", "--set-upstream", "origin", "main")
                self.git(repo, "switch", "-c", "codex/caller")
                dirty = repo / "caller-dirty.txt"
                dirty.write_text("preserve me\n", encoding="utf-8")
                caller_branch = self.git(repo, "branch", "--show-current")
                caller_status = self.git(repo, "status", "--porcelain", "--untracked-files=normal")

                original_command_output = MODULE.command_output

                def command_output_from_fixture(*args: str, **kwargs: object) -> str:
                    kwargs.setdefault("cwd", repo)
                    return original_command_output(*args, **kwargs)

                with (
                    mock.patch.object(MODULE, "ROOT", repo),
                    mock.patch.object(MODULE, "command_output", side_effect=command_output_from_fixture),
                ):
                    if body_raises:
                        with self.assertRaisesRegex(RuntimeError, "simulated lane failure"):
                            with MODULE.exact_worktree(sha) as checkout:
                                branch = self.git(checkout, "branch", "--show-current")
                                self.assertRegex(branch, r"^codex/local-ci-[0-9a-f]{32}$")
                                self.assertEqual(self.git(checkout, "rev-parse", "HEAD"), sha)
                                self.assertEqual(
                                    self.git(checkout, "status", "--porcelain", "--untracked-files=normal"),
                                    "",
                                )
                                guard_result = MODULE.subprocess.run(
                                    ["bash", str(checkout / "scripts/internal/check/check_main_branch.sh")],
                                    cwd=checkout,
                                    text=True,
                                    capture_output=True,
                                    check=False,
                                )
                                self.assertEqual(guard_result.returncode, 0, guard_result.stderr)
                                self.assertIn("feature branch", guard_result.stdout)
                                raise RuntimeError("simulated lane failure")
                    else:
                        with MODULE.exact_worktree(sha) as checkout:
                            branch = self.git(checkout, "branch", "--show-current")
                            self.assertRegex(branch, r"^codex/local-ci-[0-9a-f]{32}$")
                            self.assertEqual(self.git(checkout, "rev-parse", "HEAD"), sha)
                            guard_result = MODULE.subprocess.run(
                                ["bash", str(checkout / "scripts/internal/check/check_main_branch.sh")],
                                cwd=checkout,
                                text=True,
                                capture_output=True,
                                check=False,
                            )
                            self.assertEqual(guard_result.returncode, 0, guard_result.stderr)
                            self.assertIn("feature branch", guard_result.stdout)

                self.assertEqual(self.git(repo, "branch", "--show-current"), caller_branch)
                self.assertEqual(dirty.read_text(encoding="utf-8"), "preserve me\n")
                self.assertEqual(
                    self.git(repo, "status", "--porcelain", "--untracked-files=normal"),
                    caller_status,
                )
                self.assertEqual(self.git(repo, "branch", "--list", "codex/local-ci-*"), "")
                self.assertEqual(self.git(repo, "worktree", "list", "--porcelain").count("\nworktree "), 0)

    def test_failed_add_preserves_preexisting_matching_temporary_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            repo = root / "repo"
            remote = root / "origin.git"
            repo.mkdir()
            self.git(root, "init", "--initial-branch=main", str(repo))
            self.git(repo, "config", "user.email", "ci-test@example.invalid")
            self.git(repo, "config", "user.name", "CI test")
            guard = repo / "scripts/internal/check/check_main_branch.sh"
            guard.parent.mkdir(parents=True)
            guard.write_bytes((ROOT / "scripts/internal/check/check_main_branch.sh").read_bytes())
            (repo / "tracked.txt").write_text("pinned source\n", encoding="utf-8")
            self.git(repo, "add", "tracked.txt", "scripts/internal/check/check_main_branch.sh")
            self.git(repo, "commit", "-m", "fixture")
            sha = self.git(repo, "rev-parse", "HEAD")
            self.git(root, "init", "--bare", str(remote))
            self.git(repo, "remote", "add", "origin", str(remote))
            self.git(repo, "push", "--set-upstream", "origin", "main")
            fixed_id = "f" * 32
            temporary_branch = f"codex/local-ci-{fixed_id}"
            self.git(repo, "branch", temporary_branch, sha)
            self.git(repo, "switch", "-c", "codex/caller")
            dirty = repo / "caller-dirty.txt"
            dirty.write_text("preserve me\n", encoding="utf-8")
            caller_branch = self.git(repo, "branch", "--show-current")
            caller_status = self.git(repo, "status", "--porcelain", "--untracked-files=normal")

            original_command_output = MODULE.command_output

            def command_output_from_fixture(*args: str, **kwargs: object) -> str:
                kwargs.setdefault("cwd", repo)
                return original_command_output(*args, **kwargs)

            fixed_uuid = type("FixedUuid", (), {"hex": fixed_id})()
            with (
                mock.patch.object(MODULE, "ROOT", repo),
                mock.patch.object(MODULE, "command_output", side_effect=command_output_from_fixture),
                mock.patch.object(MODULE.uuid, "uuid4", return_value=fixed_uuid),
            ):
                with self.assertRaises(MODULE.subprocess.CalledProcessError) as raised:
                    with MODULE.exact_worktree(sha):
                        self.fail("git must refuse the pre-existing temporary branch")

            self.assertTrue(
                any("branch creation was not confirmed" in note for note in raised.exception.__notes__)
            )
            self.assertEqual(self.git(repo, "rev-parse", temporary_branch), sha)
            self.assertEqual(self.git(repo, "branch", "--show-current"), caller_branch)
            self.assertEqual(dirty.read_text(encoding="utf-8"), "preserve me\n")
            self.assertEqual(
                self.git(repo, "status", "--porcelain", "--untracked-files=normal"), caller_status
            )
            self.assertEqual(self.git(repo, "worktree", "list", "--porcelain").count("\nworktree "), 0)


class ManagedBinaryProbeTests(unittest.TestCase):
    def test_command_output_can_include_stderr_without_changing_default(self) -> None:
        completed = MODULE.subprocess.CompletedProcess(
            args=["codesign"], returncode=0, stdout="", stderr="Signature=adhoc"
        )
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            self.assertEqual(MODULE.command_output("codesign"), "")
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            self.assertEqual(MODULE.command_output("codesign", combined_output=True), "Signature=adhoc")

    def test_missing_marker_refuses_before_executing_legacy_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            sentinel = root / "legacy-binary-was-run"
            binary = root / "legacy-yield"
            binary.write_text(f"#!/bin/sh\ntouch {sentinel}\n", encoding="utf-8")
            binary.chmod(0o700)
            with self.assertRaisesRegex(MODULE.CiError, "without the maintenance admission v1 marker"):
                MODULE.compiled_build_info(binary)
            self.assertFalse(sentinel.exists())

    def test_marker_scan_handles_a_chunk_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = pathlib.Path(temporary) / "binary"
            marker = MODULE.ADMISSION_MARKER
            binary.write_bytes(b"x" * (1024 * 1024 - 3) + marker)
            self.assertTrue(MODULE.binary_contains_marker(binary))

    def test_failed_build_retains_output_and_record_without_publishing_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            state_dir = root / "git-state"
            state_dir.mkdir()
            checkout = root / "checkout"
            checkout.mkdir()
            run_managed = MODULE.run_managed

            @contextmanager
            def coordinator():
                yield state_dir

            @contextmanager
            def worktree(_sha: str):
                yield checkout

            def failing_child(_cargo_command, *, cwd, env, pass_fds, stdout):
                return run_managed(
                    [
                        sys.executable,
                        "-c",
                        "import sys; print('fake cargo stdout'); print('fake cargo stderr', file=sys.stderr); sys.exit(17)",
                    ],
                    cwd=cwd,
                    env=env,
                    pass_fds=pass_fds,
                    stdout=stdout,
                )

            args = type("Args", (), {"sha": "a" * 40, "channel": "test"})()
            with (
                mock.patch.object(MODULE, "ROOT", root),
                mock.patch.object(MODULE, "common_coordinator", coordinator),
                mock.patch.object(MODULE, "exact_worktree", worktree),
                mock.patch.object(MODULE, "require_maintenance_protocol"),
                mock.patch.object(MODULE, "doctor", return_value={"host": "test"}),
                mock.patch.object(
                    MODULE, "cargo_env", return_value={"CARGO_TARGET_DIR": str(root / "target")}
                ),
                mock.patch.object(MODULE, "run_managed", side_effect=failing_child),
            ):
                with self.assertRaisesRegex(MODULE.CiError, "build failed with exit 17"):
                    MODULE.build(args)

            result_files = list((state_dir / "runs" / ("a" * 40) / "build-test").glob("*/result.json"))
            self.assertEqual(len(result_files), 1)
            record = __import__("json").loads(result_files[0].read_text(encoding="utf-8"))
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["phase"], "cargo-build")
            self.assertEqual(record["commands"][0]["exit_code"], 17)
            log = pathlib.Path(record["log"]).read_text(encoding="utf-8")
            self.assertIn("fake cargo stdout", log)
            self.assertIn("fake cargo stderr", log)
            self.assertFalse((root / "artifacts/local-ci" / ("a" * 40) / "test").exists())


class NativeCheckCleanupTests(unittest.TestCase):
    def run_native_check(
        self,
        command_status: int,
        *,
        fail_lease_close: bool = False,
        signal_during_restore: bool = False,
    ) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            state_dir = root / "git-state"
            state_dir.mkdir()
            checkout = root / "checkout"
            runner = checkout / "scripts/internal/validation/verify_native_prerequisites.sh"
            runner.parent.mkdir(parents=True)
            runner.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            evidence = checkout / "artifacts/native-prerequisites/macos-source-release-policy"
            evidence.mkdir(parents=True)
            (evidence / "native.log").write_text("native evidence", encoding="utf-8")
            namespace_path = root / "namespace.lock"
            transition_path = root / "transition.lock"
            prior_sigterm_handler = signal.getsignal(signal.SIGTERM)
            real_close = os.close
            descriptors: dict[str, int] = {}
            close_calls: list[int] = []

            def prepare(_namespace: str, *, artifact: str) -> tuple[dict[str, str], int, int]:
                descriptors["namespace"] = os.open(namespace_path, os.O_CREAT | os.O_RDWR, 0o600)
                descriptors["transition"] = os.open(transition_path, os.O_CREAT | os.O_RDWR, 0o600)
                return {"transaction": "native-ci-test", "artifact": artifact}, descriptors["namespace"], descriptors["transition"]

            def tracking_close(fd: int) -> None:
                close_calls.append(fd)
                real_close(fd)
                if fail_lease_close and fd == descriptors.get("namespace"):
                    raise OSError(9, "simulated close reporting error")

            @contextmanager
            def coordinator():
                yield state_dir

            @contextmanager
            def worktree(_sha: str):
                yield checkout

            def restore_native(transaction: str) -> dict[str, object]:
                self.assertEqual(transaction, "native-ci-test")
                if signal_during_restore:
                    os.kill(os.getpid(), signal.SIGTERM)
                return {"transaction": transaction, "restored": True}

            args = type(
                "Args",
                (),
                {"sha": "b" * 40, "lane": "native", "case": "prerequisite:source-release-policy"},
            )()
            with (
                mock.patch.object(MODULE, "ROOT", root),
                mock.patch.object(MODULE, "COORDINATOR_FD", None),
                mock.patch.object(MODULE, "common_coordinator", coordinator),
                mock.patch.object(MODULE, "exact_worktree", worktree),
                mock.patch.object(MODULE, "require_maintenance_protocol"),
                mock.patch.object(MODULE, "doctor", return_value={"host": "test"}),
                mock.patch.object(MODULE, "cargo_env", return_value={"PATH": os.environ.get("PATH", "")}),
                mock.patch.object(MODULE.local_state, "prepare_clean_launch", side_effect=prepare),
                mock.patch.object(
                    MODULE.local_state,
                    "convert_clean_handoff_to_validator_lease",
                    side_effect=lambda _namespace, namespace_fd, _transition_fd: namespace_fd,
                ),
                mock.patch.object(
                    MODULE.local_state,
                    "restore",
                    side_effect=restore_native,
                ) as restore,
                mock.patch.object(MODULE.os, "close", side_effect=tracking_close),
                mock.patch.object(MODULE, "run_managed", return_value=command_status) as run,
                mock.patch.dict(MODULE.os.environ, {"YIELD_CONFIG_HOME": "", "YIELD_CONFIG_PROFILE": ""}),
            ):
                expected_exception = command_status != 0 or fail_lease_close
                if expected_exception:
                    with self.assertRaises(MODULE.CiError):
                        MODULE.check(args)
                else:
                    MODULE.check(args)

            self.assertEqual(restore.call_count, 1)
            self.assertEqual(run.call_count, 1)
            self.assertTrue(run.call_args.kwargs["preserve_ignored_on_exit"])
            self.assertIs(signal.getsignal(signal.SIGTERM), prior_sigterm_handler)
            self.assertEqual(close_calls.count(descriptors["namespace"]), 1)
            self.assertEqual(close_calls.count(descriptors["transition"]), 1)
            result_files = list(
                (state_dir / "runs" / ("b" * 40) / "native").glob(
                    "*/prerequisite:source-release-policy.json"
                )
            )
            self.assertEqual(len(result_files), 1)
            record = json.loads(result_files[0].read_text(encoding="utf-8"))
            self.assertIn("production_state_restore", record)
            self.assertTrue(record["production_state_restore"]["restored"])
            self.assertEqual(record["results"][0]["exit_code"], command_status)
            if fail_lease_close:
                self.assertEqual(record["status"], "cleanup_failed")
                self.assertEqual(record["descriptor_cleanup_errors"][0]["descriptor"], "validator lease")
                self.assertTrue((result_files[0].parent / "evidence/native.log").is_file())
            elif command_status:
                self.assertEqual(record["status"], "failed")
                self.assertFalse((result_files[0].parent / "evidence").exists())
            else:
                self.assertEqual(record["status"], "passed")
                self.assertEqual(
                    (result_files[0].parent / "evidence/native.log").read_text(encoding="utf-8"),
                    "native evidence",
                )
            return record

    def test_native_check_success_and_command_failure_restore_and_close_aliased_fd_once(self) -> None:
        self.run_native_check(0)
        self.run_native_check(17)

    def test_native_check_descriptor_close_error_still_restores_and_records_evidence(self) -> None:
        self.run_native_check(0, fail_lease_close=True)

    def test_native_check_late_sigterm_during_restore_completes_and_restores_handlers(self) -> None:
        self.run_native_check(0, signal_during_restore=True)


if __name__ == "__main__":
    unittest.main()
