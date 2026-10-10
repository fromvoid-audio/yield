"""Focused trust-boundary tests for browser workload captures and regression gates."""

import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gpui_browser_view_model as workload


def record(value):
    return {
        "schema": "yield.browser-vm.v1",
        "entries": 20_000,
        "iterations": 30,
        "query_ns": [value] * 30,
        "sort_ns": [value] * 30,
        "collection_counts_ns": [value] * 30,
        "collection_counts": [3334, 3334, 3333, 3333, 3333, 3333],
    }


class BrowserWorkloadCaptureTests(unittest.TestCase):
    def setUp(self):
        self.host = {"cpu_model": "test host"}
        self.build = {"profile": "release-local", "workspace_config_sha256": {}}
        self.dataset = {"fixture_version": "browser-vm-20k-v1"}
        self.source = {
            "head": workload.command_output(["git", "rev-parse", "HEAD"]),
            "tracked_file_sha256": {
                "scripts/internal/perf/gpui_browser_view_model.py": "runner-sha"},
        }
        self.baseline = {
            "schema": "yield.gpui-browser-view-model.v1",
            "status": "captured_uncalibrated",
            "host": self.host,
            "build": self.build,
            "dataset": self.dataset,
            "source": self.source,
            "runs": [record(1_000_000) for _ in range(5)],
        }
        self.candidate = {
            "host": self.host,
            "build": self.build,
            "dataset": self.dataset,
            "source": self.source,
            "runs": [record(1_000_000) for _ in range(5)],
        }
        self.candidate["summary"] = workload.summarize(self.candidate["runs"])

    def compare(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "baseline.json"
            path.write_text(json.dumps(self.baseline), encoding="utf-8")
            return workload.compare_calibration(self.candidate, self.baseline, path)

    def test_zero_measurement_is_rejected(self):
        malformed = record(1)
        malformed["query_ns"][0] = 0
        with self.assertRaisesRegex(ValueError, "invalid timing sample"):
            workload.validate_record(malformed)

    def test_missing_stage_is_rejected(self):
        malformed = record(1)
        del malformed["sort_ns"]
        with self.assertRaisesRegex(ValueError, "unexpected browser workload schema"):
            workload.validate_record(malformed)

    def test_host_or_build_mismatch_refuses_calibrated_pass(self):
        self.candidate["host"] = {"cpu_model": "other host"}
        with self.assertRaisesRegex(ValueError, "host identity"):
            self.compare()
        self.candidate["host"] = self.host
        self.candidate["build"] = {"profile": "release-local", "workspace_config_sha256": {"Cargo.toml": "changed"}}
        with self.assertRaisesRegex(ValueError, "build configuration differs"):
            self.compare()

    def test_real_p95_budget_exceedance_fails(self):
        self.candidate["runs"] = [record(2_000_000) for _ in range(5)]
        self.candidate["summary"] = workload.summarize(self.candidate["runs"])
        _thresholds, status = self.compare()
        self.assertEqual(status, "failed")


if __name__ == "__main__":
    unittest.main()
