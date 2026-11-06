"""Safety checks for the managed binary compatibility probe."""

from __future__ import annotations

import importlib.util
import io
from importlib.machinery import SourceFileLoader
import json
import os
import pathlib
import plistlib
import signal
import sys
import tempfile
import unittest
from contextlib import contextmanager, redirect_stderr, redirect_stdout
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[3]
CLI = ROOT / "scripts/yield-local-ci"
SPEC = importlib.util.spec_from_loader("yield_local_ci", SourceFileLoader("yield_local_ci", str(CLI)))
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CargoValidationTargetTests(unittest.TestCase):
    @contextmanager
    def fixture(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary).resolve()
            state = base / "repo/.git/local-ci"
            state.mkdir(parents=True)
            checkout = state / "checkout"
            checkout.mkdir()
            (checkout / "Cargo.lock").write_text("validation cache input")
            external = base / "external"
            external.mkdir(mode=0o700)
            with (
                mock.patch.object(MODULE, "ROOT", state.parent.parent),
                mock.patch.dict(os.environ, {MODULE.VALIDATION_ROOT_OVERRIDE: str(external)}, clear=True),
                mock.patch.object(
                    MODULE, "command_output", return_value="release: 1.93.0\nhost: aarch64-apple-darwin"
                ),
            ):
                yield state, checkout, external

    def test_validation_uses_external_lock_key_and_packaging_keeps_its_own_root(self):
        with self.fixture() as (state, checkout, external):
            default = MODULE.cargo_env(state, "check", checkout)
            configured = MODULE.cargo_env(state, "check", checkout, external)
            package = MODULE.cargo_env(state, "release", checkout, external)
            default_target = pathlib.Path(default["CARGO_TARGET_DIR"])
            target = pathlib.Path(configured["CARGO_TARGET_DIR"])
            self.assertEqual(default_target.parent, external.parent / "yield-native-target/validation")
            self.assertEqual(target, external / default_target.name)
            self.assertEqual(configured["YIELD_VALIDATION_TARGET_ROOT"], str(external))
            self.assertEqual(target.stat().st_mode & 0o777, 0o700)
            self.assertEqual(pathlib.Path(package["CARGO_TARGET_DIR"]).parent, external.parent / "yield-native-target/release-local")
            (checkout / "Cargo.lock").write_text("different lock contents")
            self.assertNotEqual(MODULE.cargo_env(state, "check", checkout, external)["CARGO_TARGET_DIR"], str(target))

    def test_configured_root_rejects_aliases_shared_directories_and_unsupported_filesystems(self):
        with self.fixture() as (state, checkout, external):
            symlink = external.with_name("alias")
            symlink.symlink_to(external, target_is_directory=True)
            shared = external.with_name("shared")
            shared.mkdir(mode=0o755)
            inside_checkout = checkout / "target"
            inside_checkout.mkdir(mode=0o700)
            valid = {"filesystem": "apfs", "writable": True, "mount": "/Volumes/Scratch Disk"}
            with mock.patch.object(MODULE, "filesystem_probe", return_value=valid) as probe:
                self.assertEqual(MODULE.external_validation_root(checkout, state), (external, valid))
                probe.assert_called_once_with(external)
                for invalid, message in (
                    ("relative", "absolute"),
                    (symlink, "nonsymlink"),
                    (shared, "private"),
                    (inside_checkout, "outside source"),
                    (external.with_name("missing"), "cannot inspect"),
                ):
                    with (
                        self.subTest(invalid=invalid),
                        mock.patch.dict(os.environ, {MODULE.VALIDATION_ROOT_OVERRIDE: str(invalid)}),
                        self.assertRaisesRegex(MODULE.CiError, message),
                    ):
                        MODULE.external_validation_root(checkout, state)
                probe.return_value = {**valid, "filesystem": "exfat"}
                with self.assertRaisesRegex(MODULE.CiError, "writable APFS"):
                    MODULE.external_validation_root(checkout, state)
            self.assertEqual(list(external.iterdir()), [])

    def test_existing_key_symlink_is_rejected_without_writing_through_it(self):
        with self.fixture() as (state, checkout, external):
            target = pathlib.Path(MODULE.cargo_env(state, "check", checkout, external)["CARGO_TARGET_DIR"])
            target.rmdir()
            other = external.with_name("other")
            other.mkdir(mode=0o700)
            target.symlink_to(other, target_is_directory=True)
            with self.assertRaisesRegex(MODULE.CiError, "cache key must be a private"):
                MODULE.cargo_env(state, "check", checkout, external)
            self.assertTrue(target.is_symlink())
            self.assertEqual(list(other.iterdir()), [])

    def test_volume_mount_name_preserves_spaces(self):
        df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk4s1 2000 1 1999 1% /Volumes/Scratch Disk\n"
        disk = plistlib.dumps({"FilesystemType": "apfs", "WritableVolume": True})
        with mock.patch.object(MODULE.subprocess, "check_output", side_effect=[df, disk]):
            facts = MODULE.filesystem_probe(pathlib.Path("/Volumes/Scratch Disk/yield"))
        self.assertEqual(facts["mount"], "/Volumes/Scratch Disk")


class DevelopmentResetTests(unittest.TestCase):
    def test_reset_parks_orphaned_ownership_and_keeps_new_normal_state_and_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            support = pathlib.Path(temporary)
            root, control = MODULE.local_state.namespace_paths("production", support)
            development, _ = MODULE.local_state.namespace_paths("development", support)
            development.mkdir()
            (development / "preserve.txt").write_text("test build state", encoding="utf-8")
            marker = root / "volume-ownership" / "volume-123.lock"
            marker.parent.mkdir(parents=True)
            evidence = b"version=1\nstate=dirty\nowner_profile_identity=old-profile\ngeneration=1\n"
            marker.write_bytes(evidence)
            original_identity = (root.stat().st_dev, root.stat().st_ino)
            marker_inode = marker.stat().st_ino

            def invoke(*args: str) -> tuple[int, dict[str, object], str]:
                output, errors = io.StringIO(), io.StringIO()
                with (
                    mock.patch.object(sys, "argv", [str(CLI), "state", *args]),
                    mock.patch.object(MODULE.local_state, "canonical_support_dir", return_value=support),
                    redirect_stdout(output),
                    redirect_stderr(errors),
                ):
                    result = MODULE.main()
                return result, json.loads(output.getvalue()) if output.getvalue() else {}, errors.getvalue()

            with MODULE.local_state.maintenance_locks(control, "production", exclusive=False):
                code, _, error = invoke("reset", "--namespace", "production")
            self.assertEqual(code, 1)
            self.assertIn("live owner", error)
            self.assertEqual(marker.read_bytes(), evidence)

            code, result, error = invoke("reset", "--namespace", "production")
            self.assertEqual(code, 0, error)
            self.assertFalse(root.exists())
            backup = pathlib.Path(str(result["backup"]))
            retained_marker = backup / "volume-ownership" / marker.name
            self.assertEqual((backup.stat().st_dev, backup.stat().st_ino), original_identity)
            self.assertEqual(retained_marker.stat().st_ino, marker_inode)
            self.assertEqual(retained_marker.read_bytes(), evidence)
            self.assertEqual((development / "preserve.txt").read_text(), "test build state")
            code, vanilla, error = invoke("reset", "--namespace", "production")
            self.assertEqual(code, 0, error)
            self.assertIsNone(vanilla["transaction"])
            self.assertIn("no backup was created", vanilla["restore_note"])
            self.assertEqual(retained_marker.read_bytes(), evidence)

            # Ordinary launches create their new state; status must not reset it again.
            preferences = root / "profiles" / "gpui-development" / "preferences.json"
            preferences.parent.mkdir(parents=True)
            preferences.write_text('{"output_volume":0.1}', encoding="utf-8")
            for _ in range(2):
                code, current, error = invoke("status", "--namespace", "production")
                self.assertEqual(code, 0, error)
                self.assertEqual(current["state"], "present")
                self.assertEqual(preferences.read_text(), '{"output_volume":0.1}')
            code, restored, error = invoke("restore", "--backup", str(result["transaction"]))
            self.assertEqual(code, 0, error)
            self.assertEqual(marker.read_bytes(), evidence)
            self.assertEqual(marker.stat().st_ino, marker_inode)
            displaced = pathlib.Path(str(restored["clean_session"]))
            self.assertEqual((displaced / preferences.relative_to(root)).read_text(), '{"output_volume":0.1}')


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
        case: str = "prerequisite:source-release-policy",
        redirect_evidence: bool = False,
        write_redirected_evidence: bool = True,
    ) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            state_dir = root / "git-state"
            state_dir.mkdir()
            checkout = root / "checkout"
            runner_name = (
                "verify_macos_audition_coreaudio.sh"
                if case == "audition-coreaudio"
                else "verify_native_prerequisites.sh"
            )
            runner = checkout / "scripts/internal/validation" / runner_name
            runner.parent.mkdir(parents=True)
            runner_source = "#!/bin/sh\n"
            if redirect_evidence:
                runner_source += 'output="${YIELD_NATIVE_VERIFICATION_OUTPUT:-unused}"\n'
            runner.write_text(runner_source + "exit 0\n", encoding="utf-8")
            evidence = (
                checkout / "artifacts/macos-audition-coreaudio"
                if case == "audition-coreaudio"
                else checkout / "artifacts/native-prerequisites" / f"macos-{case.partition(':')[2]}"
            )
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
                if fd in descriptors.values():
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

            def run_validator(*_args, env, **_kwargs):
                if redirect_evidence:
                    output = pathlib.Path(env["YIELD_NATIVE_VERIFICATION_OUTPUT"])
                    self.assertTrue(output.is_relative_to(state_dir / "runs"))
                    self.assertFalse(output.exists())
                    if write_redirected_evidence:
                        output.mkdir()
                        (output / "native.log").write_text("fresh redirected evidence", encoding="utf-8")
                else:
                    self.assertNotIn("YIELD_NATIVE_VERIFICATION_OUTPUT", env)
                return command_status

            args = type(
                "Args",
                (),
                {"sha": "b" * 40, "lane": "native", "case": case},
            )()
            with (
                mock.patch.object(MODULE, "ROOT", root),
                mock.patch.object(MODULE, "COORDINATOR_FD", None),
                mock.patch.object(MODULE, "common_coordinator", coordinator),
                mock.patch.object(MODULE, "exact_worktree", worktree),
                mock.patch.object(MODULE, "require_maintenance_protocol"),
                mock.patch.object(MODULE, "doctor", return_value={"host": "test"}),
                mock.patch.object(
                    MODULE,
                    "cargo_env",
                    side_effect=lambda *_args: {
                        "PATH": os.environ.get("PATH", ""),
                        "YIELD_NATIVE_VERIFICATION_OUTPUT": str(root / "ambient-stale-evidence"),
                        **(
                            {"YIELD_TEST_AUDIO_OUTPUT": os.environ["YIELD_TEST_AUDIO_OUTPUT"]}
                            if "YIELD_TEST_AUDIO_OUTPUT" in os.environ
                            else {}
                        ),
                    },
                ),
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
                mock.patch.object(MODULE, "run_managed", side_effect=run_validator) as run,
                mock.patch.dict(
                    MODULE.os.environ,
                    {
                        "YIELD_CONFIG_HOME": "",
                        "YIELD_CONFIG_PROFILE": "",
                        **({"YIELD_TEST_AUDIO_OUTPUT": "1"} if case == "audition-coreaudio" else {}),
                    },
                ),
            ):
                expected_exception = (
                    command_status != 0 or fail_lease_close
                    or (redirect_evidence and not write_redirected_evidence)
                )
                if expected_exception:
                    with self.assertRaises(MODULE.CiError):
                        MODULE.check(args)
                else:
                    MODULE.check(args)

            self.assertEqual(restore.call_count, 1)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0], ["bash", str(runner)])
            if case.startswith("prerequisite:"):
                self.assertEqual(
                    run.call_args.kwargs["env"]["YIELD_NATIVE_PREREQUISITE_CASE"],
                    case.partition(":")[2],
                )
            self.assertEqual(
                run.call_args.kwargs["env"].get("YIELD_TEST_AUDIO_OUTPUT"),
                "1" if case == "audition-coreaudio" else None,
            )
            self.assertTrue(run.call_args.kwargs["preserve_ignored_on_exit"])
            self.assertIs(signal.getsignal(signal.SIGTERM), prior_sigterm_handler)
            self.assertEqual(close_calls.count(descriptors["namespace"]), 1)
            self.assertEqual(close_calls.count(descriptors["transition"]), 1)
            result_files = list(
                (state_dir / "runs" / ("b" * 40) / "native").glob(f"*/{case}.json")
            )
            self.assertEqual(len(result_files), 1)
            record = json.loads(result_files[0].read_text(encoding="utf-8"))
            self.assertIn("production_state_restore", record)
            self.assertTrue(record["production_state_restore"]["restored"])
            self.assertEqual(record["results"][0]["exit_code"], command_status)
            if redirect_evidence:
                retained = result_files[0].parent / "evidence"
                if write_redirected_evidence:
                    self.assertEqual(record["evidence_directory"], str(retained))
                    self.assertEqual(
                        (retained / "native.log").read_text(encoding="utf-8"),
                        "fresh redirected evidence",
                    )
                    self.assertEqual(record["status"], "failed" if command_status else "passed")
                else:
                    self.assertIn("expected evidence directory", record["error"])
                    self.assertFalse(retained.exists())
                self.assertEqual(
                    (evidence / "native.log").read_text(encoding="utf-8"), "native evidence"
                )
            elif fail_lease_close:
                self.assertEqual(record["status"], "cleanup_failed")
                self.assertEqual(record["descriptor_cleanup_errors"][0]["descriptor"], "validator lease")
                self.assertTrue((result_files[0].parent / "evidence/native.log").is_file())
            elif command_status:
                self.assertEqual(record["status"], "failed")
                if case == "audition-coreaudio":
                    self.assertTrue((result_files[0].parent / "evidence/native.log").is_file())
                else:
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

    def test_audition_failure_restores_and_retains_runner_evidence(self) -> None:
        record = self.run_native_check(17, case="audition-coreaudio")
        self.assertEqual(record["status"], "failed")
        self.assertIn("production_state_restore", record)

    def test_native_redirect_retains_fresh_evidence_even_on_failure(self) -> None:
        self.run_native_check(0, redirect_evidence=True)
        self.run_native_check(17, redirect_evidence=True)

    def test_native_redirect_rejects_a_pass_without_fresh_evidence(self) -> None:
        self.run_native_check(0, redirect_evidence=True, write_redirected_evidence=False)

    def test_registration_generation_case_uses_native_handoff_and_retains_evidence(self) -> None:
        self.run_native_check(0, case="prerequisite:source-registration-generation", redirect_evidence=True)

    def test_retirement_inventory_case_uses_native_handoff_and_retains_evidence(self) -> None:
        self.run_native_check(0, case="prerequisite:registered-retirement-inventory", redirect_evidence=True)

    def test_audition_refuses_before_coordinator_or_clean_state_without_opt_in(self) -> None:
        args = type(
            "Args",
            (),
            {"sha": "c" * 40, "lane": "native", "case": "audition-coreaudio"},
        )()
        with (
            mock.patch.dict(MODULE.os.environ, {}, clear=True),
            mock.patch.object(MODULE, "common_coordinator") as coordinator,
            mock.patch.object(MODULE.local_state, "prepare_clean_launch") as prepare,
        ):
            with self.assertRaisesRegex(MODULE.CiError, "inherited YIELD_TEST_AUDIO_OUTPUT=1"):
                MODULE.check(args)
        coordinator.assert_not_called()
        prepare.assert_not_called()


if __name__ == "__main__":
    unittest.main()
