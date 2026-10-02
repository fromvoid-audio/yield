#!/usr/bin/env python3
"""Intake and process-budget negatives; these are not physical qualification."""

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import run_source_owner_workload as runner


def record():
    measured = {"status": "measured", "capacity_claim": False, "logical_page_count": 227,
                "main_retained_vfs_bytes": 8192, "wal_retained_vfs_bytes": 5141792,
                "shm_root_observed_bytes": 32768, "page_size": 4096}
    def distribution(count):
        return {"unit": "ns", "count": count, "samples": list(range(1, count + 1)),
                "p50": (count + 1) // 2, "p95": (count * 95 + 99) // 100, "max": count}
    def queue(count):
        return {"attempts": count, "accepted": count, "dequeued": count,
                "full": 0, "disconnected": 0, "pending_dispatch": 0,
                "peak_pending_dispatch": 1, "dispatch_wait_micros": count,
                "max_dispatch_wait_micros": 1, "dispatch_wait_buckets": [count] + [0] * 7}
    return {**runner.FIXTURE, "curation": {**runner.CURATION,
                "timings": {name: 1 for name in runner.CURATION_TIMINGS},
                "storage_after_commit": copy.deepcopy(measured)},
            "storage": measured, "page_queue": queue(65), "reopened_page_queue": queue(66),
            "timings": {"open_ns": 1, "audit_ns": 1, "close_ns": 1, "reopen_ns": 1,
                "manifest_batches": distribution(64), "pages": distribution(65),
                "reopened_pages": distribution(65)}, "limits": ["synthetic contract record only"]}


def result(sample, code=0):
    return subprocess.CompletedProcess([], code, runner.MARKER + json.dumps(sample) + "\n", "")


class EvidenceIntake(unittest.TestCase):
    def test_complete_and_explicitly_unavailable_storage_are_accepted(self):
        sample = record()
        self.assertEqual(runner.parse_evidence(result(sample)), sample)
        sample["storage"] = {"status": "unavailable", "capacity_claim": False,
                             "reason": "retained-storage-observation-unavailable"}
        self.assertEqual(runner.parse_evidence(result(sample)), sample)

    def test_partial_duplicate_failed_and_malformed_process_records_are_rejected(self):
        valid = result(record())
        outputs = ["", valid.stdout + valid.stdout, runner.MARKER + "{",
                   valid.stdout.replace('"rows": 4096', '"rows": 4096, "rows": 4096')]
        for output in outputs:
            with self.subTest(output=output[:80]), self.assertRaises(ValueError):
                runner.parse_evidence(subprocess.CompletedProcess([], 0, output, ""))
        with self.assertRaisesRegex(ValueError, "process failed"):
            runner.parse_evidence(result(record(), 1))

    def test_receipt_and_measurement_proof_gaps_are_rejected(self):
        cases = []
        for key in ("exact_replay", "historical_receipt_equal", "superseded_request_rejected"):
            sample = record(); sample["curation"][key] = False; cases.append(sample)
        sample = record(); sample["curation"]["committed_revision"] = 65; cases.append(sample)
        sample = record(); del sample["curation"]["timings"]["close_ns"]; cases.append(sample)
        sample = record(); sample["timings"]["pages"]["samples"].pop(); cases.append(sample)
        sample = record(); sample["timings"]["pages"]["p95"] = 0; cases.append(sample)
        sample = record(); sample["page_queue"]["dispatch_wait_buckets"].pop(); cases.append(sample)
        sample = record(); sample["page_queue"]["pending_dispatch"] = 1; cases.append(sample)
        sample = record(); sample["timings"]["open_ns"] = -1; cases.append(sample)
        sample = record(); sample["timings"]["open_ns"] = float("nan"); cases.append(sample)
        sample = record(); sample["timings"]["open_ns"] = 1 << 64; cases.append(sample)
        sample = record(); sample["rows"] = True; cases.append(sample)
        sample = record(); sample["storage"]["capacity_claim"] = True; cases.append(sample)
        sample = record(); sample["request_path"] = "/unexpected/history"; cases.append(sample)
        for index, sample in enumerate(cases):
            with self.subTest(case=index), self.assertRaises((ValueError, KeyError)):
                runner.parse_evidence(result(sample))

    def test_checkout_and_build_overrides_are_rejected_without_exposing_values(self):
        with patch.object(runner, "capture", return_value=" M Cargo.toml"):
            with self.assertRaisesRegex(ValueError, "clean checkout"):
                runner.checkout()
        for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_PROFILE_TEST_OPT_LEVEL", "RUSTC_WRAPPER",
                     "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER"):
            with self.subTest(name=name), self.assertRaises(ValueError) as error:
                runner.build_configuration({name: "private-value-must-not-be-printed"})
            self.assertNotIn("private-value", str(error.exception))
        with tempfile.TemporaryDirectory() as directory, patch.object(runner, "ROOT", Path(directory)):
            self.assertIsInstance(runner.build_configuration({"CARGO_TARGET_DIR": directory}), dict)

    def test_changed_checkout_and_configuration_cannot_complete_bundle(self):
        for heads, configurations in ((["a", "a", "b"], [{}, {}]), (["a", "a", "a"], [{}, {}, {"changed": "hash"}])):
            with patch.object(runner, "checkout", side_effect=heads), \
                 patch.object(runner, "build_configuration", side_effect=configurations), \
                 patch.object(runner, "capture", return_value="fixture-version"), \
                 patch.object(runner, "run_bounded", return_value=result(record())):
                with self.assertRaisesRegex(ValueError, "state changed"):
                    runner.collect(2)

    def test_failed_collection_does_not_publish_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "evidence.json"
            with patch.object(sys, "argv", ["runner", "--output", str(output)]), \
                 patch.object(runner, "collect", side_effect=ValueError("partial record")):
                with self.assertRaises(ValueError):
                    runner.main()
            self.assertFalse(output.exists())


class ProcessBudgets(unittest.TestCase):
    def test_separate_streams_and_exit_status_are_preserved(self):
        value = runner.run_bounded([sys.executable, "-c", "import sys; print('out'); print('err',file=sys.stderr); sys.exit(7)"])
        self.assertEqual((value.returncode, value.stdout.strip(), value.stderr.strip()), (7, "out", "err"))

    def test_runaway_output_and_timeout_fail_closed(self):
        with self.assertRaisesRegex(RuntimeError, "output budget"):
            runner.run_bounded([sys.executable, "-c", "print('x' * 100000)"], output_limit=1024)
        with self.assertRaisesRegex(RuntimeError, "time budget"):
            runner.run_bounded([sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.1)


if __name__ == "__main__":
    unittest.main()
