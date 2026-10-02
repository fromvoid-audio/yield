#!/usr/bin/env python3
"""Collector negatives are contract tests; the separate actual collector run is evidence."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import global_recovery_evidence as evidence


def marker(case=None):
    return (evidence.PREFIX + json.dumps(evidence.CASE if case is None else case) + "\n").encode()


def binding():
    names = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", ".cargo/config.toml",
             ".cargo/config", "crates/yield-library/Cargo.toml", "cargo_home_config", "cargo_home_config_toml")
    return {"head": "a" * 40, "tree": "b" * 40, "dirty": False,
            "configuration_sha256": dict.fromkeys(names, None)}


def artifact(path="/unused/compiled-test"):
    return json.dumps({"reason": "compiler-artifact", "target": {"name": evidence.TARGET,
         "kind": ["test"], "src_path": str(evidence.ROOT / evidence.FIXTURE)},
         "features": [], "profile": {"test": True}, "executable": path}).encode()


def simulated_collection():
    # Fabricated subprocess records exercise validation only, not process recovery.
    build = subprocess.CompletedProcess([], 0, artifact().decode(), "private build paths")
    run = subprocess.CompletedProcess([], 0, marker().decode(), "")
    return build, run


class Intake(unittest.TestCase):
    def test_case_requires_marker_and_success(self):
        self.assertEqual(evidence.parse_case(marker(), b"", 0), evidence.CASE)
        for stdout, stderr, code in ((b"", b"", 0), (marker(), b"", 1),
                                     (marker() * 2, b"", 0), (marker(), marker(), 0),
                                     (b"noise" + marker(), b"", 0),
                                     (marker(), b"x" * evidence.LOG_LIMIT, 0)):
            with self.subTest(code=code, size=len(stdout)), self.assertRaises(ValueError):
                evidence.parse_case(stdout, stderr, code)

    def test_duplicate_unknown_malformed_oversized_and_unredacted_records_reject(self):
        records = [b'{"schema":1,"schema":2}', b'{', b'{} trailing', b'[' * 3000,
                   b'x' * (evidence.RECORD_LIMIT + 1)]
        for key, value in (("owner", "private-owner"), ("root", "/private/path"),
                           ("observations", ["token=private"]), ("outcome", "qualified"),
                           ("provenance", "synthetic"), ("schema", "v2")):
            case = copy.deepcopy(evidence.CASE)
            case[key] = value
            records.append(json.dumps(case).encode())
        for record in records:
            with self.subTest(record=record[:30]), self.assertRaises(ValueError):
                evidence.parse_case(evidence.PREFIX.encode() + record + b"\n", b"", 0)

    def test_compiled_artifact_must_be_exact_normal_build(self):
        good = artifact()
        self.assertEqual(evidence.artifact(good), Path("/unused/compiled-test"))
        variants = [b"", good + b"\n" + good, b"x" * (evidence.BUILD_LIMIT + 1)]
        for change in ({"features": ["test-support"]}, {"profile": {"test": False}},
                       {"target": {"name": evidence.TARGET, "kind": ["lib"], "src_path": "wrong"}},
                       {"executable": None}):
            item = json.loads(good)
            item.update(change)
            variants.append(json.dumps(item).encode())
        item = json.loads(good)
        item["target"]["src_path"] = "/private/unrelated.rs"
        variants.append(json.dumps(item).encode())
        for variant in variants:
            with self.assertRaises(ValueError):
                evidence.artifact(variant)

    def test_actual_binary_digest_is_bounded_and_rejects_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "binary"
            path.write_bytes(b"actual test bytes")
            self.assertEqual(len(evidence.binary_digest(path)), 64)
            alias = path.with_name("alias")
            alias.symlink_to(path)
            with self.assertRaises(ValueError):
                evidence.binary_digest(alias)
            with patch.object(evidence, "BINARY_LIMIT", 2), self.assertRaises(ValueError):
                evidence.binary_digest(path)

    def collect_simulated(self, bindings=None, hashes=None, results=None):
        with patch.object(evidence, "source_binding", side_effect=bindings or [binding()] * 3), \
             patch.object(evidence, "binary_digest", side_effect=hashes or ["c" * 64] * 2), \
             patch.object(evidence.runtime, "capture", return_value="private tool version"), \
             patch.object(evidence.runtime, "run_bounded", side_effect=results or simulated_collection()), \
             patch.object(evidence.platform, "system", return_value="Linux"), \
             patch.object(evidence.platform, "release", return_value="6.1.0"), \
             patch.object(evidence.platform, "machine", return_value="x86_64"):
            return evidence.collect()

    def test_report_schema_redacts_paths_and_requires_all_scoped_limits(self):
        encoded = self.collect_simulated()
        report = evidence.validate_report(encoded)
        self.assertNotIn(b"private", encoded)
        self.assertNotIn(b"/unused", encoded)
        for key in ("unexercised", "qualification", "case"):
            altered = copy.deepcopy(report)
            altered[key] = [] if key == "unexercised" else "qualified"
            with self.assertRaises(ValueError):
                evidence.validate_report(json.dumps(altered).encode())
        altered = copy.deepcopy(report)
        altered["source"]["dirty"] = True
        with self.assertRaises(ValueError):
            evidence.validate_report(json.dumps(altered).encode())
        altered = copy.deepcopy(report)
        altered["compiled_test"]["source_tree"] = "d" * 40
        with self.assertRaises(ValueError):
            evidence.validate_report(json.dumps(altered).encode())
        altered = copy.deepcopy(report)
        altered["run"]["exit_status"] = False  # False must not equal integer success.
        with self.assertRaises(ValueError):
            evidence.validate_report(json.dumps(altered).encode())
        with self.assertRaises(ValueError):
            evidence.validate_report(encoded.replace(b'"status":', b'"unexpected":1,"status":'))
        with self.assertRaises(ValueError):
            evidence.validate_report(encoded.replace(b'"dirty":false', b'"dirty":false,"dirty":false'))
        with self.assertRaises(ValueError):
            evidence.validate_report(b" " * (evidence.REPORT_LIMIT + 1))

    def test_dirty_changed_source_config_and_stale_binary_cannot_pass(self):
        changed = binding()
        changed["tree"] = "d" * 40
        for bindings, hashes in (([binding(), changed], None), ([binding(), binding(), changed], None),
                                (None, ["c" * 64, "d" * 64])):
            with self.assertRaises(ValueError):
                self.collect_simulated(bindings=bindings, hashes=hashes)
        with patch.object(evidence.runtime, "capture", return_value=" M changed"):
            with self.assertRaisesRegex(ValueError, "clean checkout"):
                evidence.source_binding()
        for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_PROFILE_TEST_OPT_LEVEL"):
            with self.assertRaises(ValueError):
                evidence.runtime.build_configuration({name: "private-secret"})

    def test_failed_build_and_failed_run_never_publish_success(self):
        build, run = simulated_collection()
        for results in ([subprocess.CompletedProcess([], 1, "", "private failure")],
                        [build, subprocess.CompletedProcess([], 1, run.stdout, "private failure")]):
            with self.assertRaises(ValueError):
                self.collect_simulated(results=results)
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(sys, "argv", ["collector", "--output", directory + "/report.json"]), \
             patch.object(evidence, "collect", side_effect=RuntimeError("private failure")), \
             patch.object(evidence.argparse.ArgumentParser, "exit", side_effect=SystemExit(2)):
            with self.assertRaises(SystemExit):
                evidence.main()
            self.assertFalse((Path(directory) / "report.json").exists())

    def test_owned_subprocess_failure_timeout_and_output_budget(self):
        evidence.runtime.ROOT = evidence.ROOT
        failed = evidence.runtime.run_bounded([sys.executable, "-c", "raise SystemExit(3)"], timeout=5)
        self.assertEqual(failed.returncode, 3)
        with self.assertRaisesRegex(RuntimeError, "output budget"):
            evidence.runtime.run_bounded([sys.executable, "-c", "print('x'*4096)"],
                                         timeout=5, output_limit=32)
        # Runner kills/reaps its group, including a descendant that keeps pipes open.
        code = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']);time.sleep(30)"
        with self.assertRaisesRegex(RuntimeError, "time budget"):
            evidence.runtime.run_bounded([sys.executable, "-c", code], timeout=0.1)

    def test_publication_is_complete_exclusive_and_cleans_owned_staging(self):
        encoded = self.collect_simulated()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            evidence.publish(output, encoded)
            self.assertEqual(output.read_bytes(), encoded)
            with self.assertRaises(FileExistsError):
                evidence.publish(output, encoded)
            self.assertEqual(list(Path(directory).iterdir()), [output])
            failed = Path(directory) / "failed.json"
            with patch.object(evidence.os, "link", side_effect=OSError("private failure")):
                with self.assertRaises(OSError):
                    evidence.publish(failed, encoded)
            self.assertFalse(failed.exists())
            self.assertEqual(list(Path(directory).iterdir()), [output])


if __name__ == "__main__":
    unittest.main()
