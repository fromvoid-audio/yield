"""Regression checks for trustworthy bounded baseline evidence."""

import json
from pathlib import Path
import sys
import tempfile
import unittest

import gpui_baseline as baseline


def fixture_log():
    lines = []
    for kind, inputs in sorted(baseline.EXPECTED):
        geometry = kind == "geometry"
        lines.append(baseline.PREFIX + json.dumps({
            "schema": "yield.waveform-geometry.v1", "kind": kind,
            "input_columns": inputs, "width_px": 640 if geometry else 1120,
            "height_px": 180, "miss_iterations": 100 if geometry else 8,
            "hit_iterations": 100, "miss_total_ns": 12345, "hit_total_ns": 123,
        }))
    return "\n".join(lines)


class BaselineTests(unittest.TestCase):
    def test_zero_matched_tests_does_not_capture_success(self):
        with self.assertRaisesRegex(ValueError, "missing or duplicate"):
            baseline.parse_records("test result: ok. 0 passed; 0 failed")

    def test_requires_all_cases_exactly_once(self):
        log = fixture_log()
        self.assertEqual(len(baseline.parse_records(log)), 5)
        for invalid in ("\n".join(log.splitlines()[1:]),
                        "\n".join(log.splitlines()[:-1] + [log.splitlines()[0]]),
                        log + "\n" + log.splitlines()[0]):
            with self.subTest(log=invalid), self.assertRaises(ValueError):
                baseline.parse_records(invalid)

    def test_rejects_invalid_numbers_and_changed_configuration(self):
        log = fixture_log()
        for old, new in (("12345", "true"), ("12345", "-1"),
                         ("12345", "NaN"), ('"height_px": 180', '"height_px": 1')):
            with self.subTest(new=new), self.assertRaises(ValueError):
                baseline.parse_records(log.replace(old, new, 1))

    def test_rejects_unbounded_or_unknown_records(self):
        for invalid in (baseline.PREFIX + "x" * baseline.MAX_RECORD_BYTES,
                        fixture_log().replace("waveform-geometry.v1", "waveform-geometry.v2"),
                        fixture_log().replace('"kind": "fade"', '"kind": "unknown"', 1)):
            with self.subTest(log=invalid), self.assertRaises(ValueError):
                baseline.parse_records(invalid)

    def test_rejects_duplicate_json_keys(self):
        log = fixture_log().replace('"height_px": 180', '"height_px": 1, "height_px": 180', 1)
        with self.assertRaisesRegex(ValueError, "duplicate benchmark field"):
            baseline.parse_records(log)

    def test_failed_command_cannot_publish_valid_looking_records(self):
        code = "print(%r); raise SystemExit(1)" % fixture_log()
        with self.assertRaisesRegex(ValueError, "command failed"):
            baseline.run_benchmark([sys.executable, "-c", code], 5)

    def test_hung_child_is_killed_on_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "survived"
            child = "import time; time.sleep(1); open(%r, 'w').close()" % str(marker)
            code = ("import subprocess, sys, time; "
                    "subprocess.Popen([sys.executable, '-c', %r]); time.sleep(30)" % child)
            with self.assertRaisesRegex(ValueError, "timeout"):
                baseline.run_benchmark([sys.executable, "-c", code], 0.1)
            # The parent and child share a killed process group; no delayed mutation survives.
            import time
            time.sleep(1.1)
            self.assertFalse(marker.exists())

    def test_log_flood_is_bounded(self):
        code = "import sys; sys.stdout.write('x' * %d)" % (baseline.MAX_LOG_BYTES + 1)
        with self.assertRaisesRegex(ValueError, "byte limit"):
            baseline.run_benchmark([sys.executable, "-c", code], 5)


if __name__ == "__main__":
    unittest.main()
