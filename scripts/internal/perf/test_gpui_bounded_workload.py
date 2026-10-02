import unittest

import gpui_bounded_workload as workload


class CaptureTests(unittest.TestCase):
    def test_duplicate_fields_are_not_silently_overwritten(self):
        with self.assertRaisesRegex(ValueError, "duplicate workload field"):
            workload.parse_records('YIELD_WORKLOAD={"kind":"file-owner","cycle":0,"cycle":1}')

    def test_negative_or_boolean_measurements_are_rejected(self):
        for value in ("-1", "true"):
            with self.assertRaises(ValueError):
                workload.parse_records('YIELD_WORKLOAD={"kind":"file-owner","cycle":' + value + '}')

    def test_empty_capture_cannot_be_reported_as_success(self):
        with self.assertRaisesRegex(ValueError, "capture incomplete"):
            workload.validate([])

    def test_unknown_window_kind_cannot_be_reported_as_success(self):
        with self.assertRaisesRegex(ValueError, "unknown workload kind"):
            workload.validate([{"kind": "native-frame-pass"}])
