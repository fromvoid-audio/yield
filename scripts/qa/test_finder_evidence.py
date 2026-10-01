"""Portable regressions for redacted, fail-closed Finder evidence collection."""

import copy
import json
import unittest

import finder_evidence as evidence


def model_records():
    valid = {
        "schema": "yield.finder-continuity-evidence.v1", "case": "valid_finder_replay",
        "source_identity_ref": "fixture-source-01", "root_identity_ref": "fixture-root-01",
        "stream_identity": {"backend": "fsevents", "device": 17,
                            "volume_uuid": "redacted-fixture-01", "matched": True},
        "audit_marker_stream_identity": "not_applicable", "lifecycle_generation": 7,
        "watcher_generation": 1,
        "cursor": {"prior_ack": 100, "started_after": 100, "first_sequence": 101,
                   "last_sequence": 106, "durable_ack_after": 106},
        "barrier": {"before": 100, "after": 106, "advanced": True},
        "covered_region": {"after": 100, "through": 106, "contiguous": True},
        "source_revision": {"before": 1, "after": 2}, "uncertainty": ["none"],
        "audit_disposition": "not_required",
        "outcome": "committed_once; durable_after_pre_ack_restart",
        "restart": "actual_profile_sqlite_reopen_before_case",
        "native_fsevents": "blocked_not_exercised_synthetic_fixture_only",
        "native_ui": "blocked_all_UI_controls_paused",
    }
    uncertain = copy.deepcopy(valid)
    uncertain.update({
        "case": "unsupported_finder_shape", "watcher_generation": 2,
        "cursor": {"prior_ack": 106, "started_after": 106, "first_sequence": 107,
                   "last_sequence": 108, "durable_ack_after": 106},
        "barrier": {"before": 106, "after": 106, "advanced": False},
        "covered_region": {"after": 106, "through": 108, "contiguous": True},
        "source_revision": {"before": 2, "after": 2}, "uncertainty": ["unknown_event"],
        "audit_disposition": "retained_source_audit",
        "audit_marker_stream_identity": "omitted_source_wide_marker",
        "outcome": "checkpoint_not_advanced; last_good_projection_retained",
    })
    return [valid, uncertain]


def log(records):
    return "\n".join(evidence.PREFIX + json.dumps(record) for record in records).encode()


class FinderEvidenceTests(unittest.TestCase):
    def test_complete_matrix_has_explicit_native_and_io_gaps(self):
        fixture = (evidence.ROOT / evidence.FIXTURE).read_bytes()
        result = evidence.report("a" * 40, fixture, log(model_records()))
        self.assertEqual(result["status"], "blocked_native_acceptance")
        self.assertEqual(len(result["sessions"]), 11)
        self.assertEqual(result["model_status"], "validated_synthetic_replay_only")
        for session in result["sessions"]:
            self.assertTrue(session["native"].startswith("blocked"))
            self.assertTrue(session["no_ui_io"].startswith("unverified"))
        encoded = json.dumps(result)
        self.assertNotIn("kick.wav", encoded)
        self.assertNotIn("Library/", encoded)
        self.assertNotIn("snare.wav", encoded)

    def test_no_input_keeps_model_and_per_session_revisions_unknown(self):
        fixture = (evidence.ROOT / evidence.FIXTURE).read_bytes()
        result = evidence.report("a" * 40, fixture)
        self.assertEqual(result["model_status"], "not_captured")
        self.assertTrue(all(row["batch_source_revision"] is None for row in result["sessions"]))

    def test_unknown_fields_and_path_leaks_are_rejected(self):
        for field in ("path", "ui_path", "new_unknown_field"):
            records = model_records()
            records[0][field] = "/Users/example/private/sample.wav"
            with self.subTest(field=field), self.assertRaises(ValueError):
                evidence.parse_records(log(records))
        records = model_records()
        records[0]["root_identity_ref"] = "/private/root"
        with self.assertRaisesRegex(ValueError, "unredacted"):
            evidence.parse_records(log(records))

    def test_synthetic_native_success_is_rejected(self):
        records = model_records()
        records[0]["native_ui"] = "passed"
        with self.assertRaisesRegex(ValueError, "cannot claim native"):
            evidence.parse_records(log(records))

    def test_uncertain_replay_cannot_advance_cursor_or_revision(self):
        for mutate in (lambda row: row["source_revision"].update(after=3),
                       lambda row: row["barrier"].update(advanced=True),
                       lambda row: row["cursor"].update(durable_ack_after=108)):
            records = model_records()
            mutate(records[1])
            with self.assertRaises(ValueError):
                evidence.parse_records(log(records))

    def test_coverage_and_generation_fences(self):
        for mutate in (lambda row: row["covered_region"].update(through=105),
                       lambda row: row["cursor"].update(started_after=99),
                       lambda row: row.update(watcher_generation=0),
                       lambda row: row["source_revision"].update(after=1),
                       lambda row: row["cursor"].update(prior_ack=True)):
            records = model_records()
            mutate(records[0])
            with self.assertRaises(ValueError):
                evidence.parse_records(log(records))

    def test_missing_duplicate_or_unordered_cases_fail(self):
        records = model_records()
        for invalid in ([records[0]], [records[0], records[0]], records + [records[1]], records[::-1]):
            with self.assertRaises(ValueError):
                evidence.parse_records(log(invalid))

    def test_duplicate_keys_and_large_records_fail(self):
        data = log(model_records()).replace(b'"watcher_generation": 1', b'"watcher_generation": 0, "watcher_generation": 1')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            evidence.parse_records(data)
        with self.assertRaisesRegex(ValueError, "byte limit"):
            evidence.parse_records((evidence.PREFIX + "x" * 2048).encode())

    def test_fixture_rejects_traversal_absolute_paths_and_missing_actions(self):
        fixture = (evidence.ROOT / evidence.FIXTURE).read_bytes()
        for invalid in (fixture.replace(b"Library/kick.wav", b"/Users/private/kick.wav"),
                        fixture.replace(b"kick.wav", b"../kick.wav"),
                        b"\n".join(fixture.splitlines()[:-1])):
            with self.assertRaises(ValueError):
                evidence.fixture_rows(invalid)


if __name__ == "__main__":
    unittest.main()
