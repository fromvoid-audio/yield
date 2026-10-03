#!/usr/bin/env python3
"""Collect existing redacted Finder model evidence without claiming native acceptance."""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = "crates/yield-source-watcher/src/replay/fixtures/finder_v1.tsv"
PREFIX = "YIELD_FINDER_CONTINUITY_EVIDENCE="
MAX_INPUT_BYTES = 1024 * 1024
MAX_RECORD_BYTES = 2048
TEST = "replay::macos::tests::disposable_profile_reports_finder_replay_continuity_and_audit"
SESSIONS = {
    "create": ("OPT-1823", "one_current_revision"),
    "duplicate": ("OPT-1823", "distinct_file_one_publication"),
    "rename": ("OPT-1823", "identity_retained_correct_parent"),
    "reparent": ("OPT-1825", "identity_retained_correct_parent"),
    "delete": ("OPT-1825", "delete_only_with_complete_evidence"),
    "unsupported": ("OPT-1822", "retain_last_good_and_audit"),
    "empty_folder": ("OPT-1827", "publish_existing_empty_directory"),
    "unsupported_only_folder": ("OPT-1827", "retain_directory_without_audio"),
    "overflow": ("OPT-1829", "retain_last_good_and_audit"),
    "restart": ("OPT-1829", "durable_prior_or_audit"),
    "scan_overlap": ("OPT-1829", "incomplete_audit_cannot_retire_barrier"),
}


def bounded_read(path, limit):
    with Path(path).open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise ValueError("input exceeded byte limit")
    return data


def unique_fields(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate evidence field")
        result[key] = value
    return result


def shape(value, fields):
    if type(value) is not dict or set(value) != set(fields.split()):
        raise ValueError("unexpected evidence fields")


def integer(value):
    if type(value) is not int or not 0 <= value < 2**64:
        raise ValueError("invalid evidence integer")
    return value


def fixture_rows(data):
    if len(data) > 8192:
        raise ValueError("fixture exceeded byte limit")
    rows = []
    expected_flags = {"create": {"created-file"}, "duplicate": {"created-file"},
                      "rename": {"renamed-file"}, "reparent": {"renamed-file"},
                      "delete": {"removed-file"}, "unsupported": {"unknown-event"},
                      "empty-folder": {"created-directory"},
                      "unsupported-only": {"created-directory", "created-file"}}
    for line in data.decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 5 or len(rows) >= 32:
            raise ValueError("invalid or unbounded redacted fixture")
        action, sequence, volume_path, flags, relative = parts
        if action not in expected_flags:
            raise ValueError("unknown fixture session")
        if not sequence.isascii() or not sequence.isdecimal():
            raise ValueError("invalid fixture cursor")
        sequence = integer(int(sequence))
        if sequence == 0 or (rows and sequence <= rows[-1]["sequence"]):
            raise ValueError("fixture cursors must advance")
        if flags not in expected_flags[action] or (relative == "-") != (action == "unsupported"):
            raise ValueError("fixture action and disposition disagree")
        for locator in (volume_path, relative):
            if locator == "-":
                continue
            if len(locator.encode()) > 256 or not re.fullmatch(r"[A-Za-z0-9 ._/-]+", locator):
                raise ValueError("invalid redacted fixture locator")
            if any(part in ("", ".", "..") for part in locator.split("/")) or PurePosixPath(locator).is_absolute():
                raise ValueError("unsafe fixture locator")
        if not volume_path.startswith("Library/") or (
            relative != "-" and volume_path != "Library/" + relative
        ):
            raise ValueError("fixture root mapping changed")
        # Only action/cursor/flag shape enters the output; no fixture paths are persisted.
        rows.append({"session": action, "sequence": sequence, "event_shape": flags})
    expected_shapes = {(action, flag) for action, flags in expected_flags.items() for flag in flags}
    if {(row["session"], row["event_shape"]) for row in rows} != expected_shapes:
        raise ValueError("fixture action matrix is incomplete")
    return rows


def validate_record(record):
    shape(record, "schema case source_identity_ref root_identity_ref stream_identity "
          "audit_marker_stream_identity lifecycle_generation watcher_generation cursor barrier "
          "covered_region source_revision uncertainty audit_disposition outcome restart native_fsevents native_ui")
    if record["schema"] != "yield.finder-continuity-evidence.v1":
        raise ValueError("unknown evidence schema")
    if record["case"] not in ("valid_finder_replay", "unsupported_finder_shape"):
        raise ValueError("unknown evidence case")
    if record["source_identity_ref"] != "fixture-source-01" or record["root_identity_ref"] != "fixture-root-01":
        raise ValueError("unredacted source/root identity")
    stream = record["stream_identity"]
    shape(stream, "backend device volume_uuid matched")
    if stream != {"backend": "fsevents", "device": 17, "volume_uuid": "redacted-fixture-01", "matched": True}:
        raise ValueError("unqualified or unredacted stream identity")
    integer(stream["device"])
    if stream["matched"] is not True:
        raise ValueError("invalid stream match")
    if integer(record["lifecycle_generation"]) != 7 or integer(record["watcher_generation"]) == 0:
        raise ValueError("invalid model generation fence")
    if record["restart"] != "actual_profile_sqlite_reopen_before_case" or record["native_fsevents"] != "blocked_not_exercised_synthetic_fixture_only" or record["native_ui"] != "blocked_all_UI_controls_paused":
        raise ValueError("existing synthetic records cannot claim native acceptance")
    cursor, barrier, revision = record["cursor"], record["barrier"], record["source_revision"]
    shape(cursor, "prior_ack started_after first_sequence last_sequence durable_ack_after")
    shape(barrier, "before after advanced")
    shape(revision, "before after")
    for value in list(cursor.values()) + [barrier["before"], barrier["after"]] + list(revision.values()):
        integer(value)
    if type(barrier["advanced"]) is not bool:
        raise ValueError("invalid barrier advance flag")
    if cursor["started_after"] != cursor["prior_ack"] or not cursor["prior_ack"] < cursor["first_sequence"] <= cursor["last_sequence"]:
        raise ValueError("invalid cursor order")
    if barrier["before"] != cursor["prior_ack"] or barrier["after"] != cursor["durable_ack_after"]:
        raise ValueError("barrier and durable cursor disagree")
    region = record["covered_region"]
    if region is not None:
        shape(region, "after through contiguous")
        integer(region["after"])
        integer(region["through"])
        if region["after"] != cursor["prior_ack"] or region["through"] < cursor["last_sequence"] or region["contiguous"] is not True:
            raise ValueError("invalid covered region")
    valid = record["case"] == "valid_finder_replay"
    if valid:
        if region is None or not barrier["advanced"] or cursor["durable_ack_after"] != cursor["last_sequence"] or revision["after"] <= revision["before"]:
            raise ValueError("valid replay lacks committed revision/coverage")
        expected = (["none"], "not_required", "not_applicable", "committed_once; durable_after_pre_ack_restart")
    else:
        if barrier["advanced"] or cursor["durable_ack_after"] != cursor["prior_ack"] or revision["after"] != revision["before"]:
            raise ValueError("uncertain replay advanced last-good state")
        expected = (["unknown_event"], "retained_source_audit", "omitted_source_wide_marker", "checkpoint_not_advanced; last_good_projection_retained")
    if (record["uncertainty"], record["audit_disposition"], record["audit_marker_stream_identity"], record["outcome"]) != expected:
        raise ValueError("unexpected replay disposition")
    return record


def parse_records(data):
    if len(data) > MAX_INPUT_BYTES:
        raise ValueError("input exceeded byte limit")
    records = []
    for line in data.decode("utf-8").splitlines():
        if not line.startswith(PREFIX):
            continue
        if len(line.encode()) > MAX_RECORD_BYTES:
            raise ValueError("evidence record exceeded byte limit")
        records.append(validate_record(json.loads(line[len(PREFIX):], object_pairs_hook=unique_fields)))
        if len(records) > 2:
            raise ValueError("too many evidence records")
    if {record["case"] for record in records} != {"valid_finder_replay", "unsupported_finder_shape"} or len(records) != 2:
        raise ValueError("both existing model cases are required exactly once")
    if records[0]["case"] != "valid_finder_replay" or records[1]["source_revision"]["before"] != records[0]["source_revision"]["after"] or records[1]["cursor"]["prior_ack"] != records[0]["cursor"]["durable_ack_after"]:
        raise ValueError("model session continuity is inconsistent")
    return records


def report(head, fixture, input_data=None):
    rows = fixture_rows(fixture)
    records = parse_records(input_data) if input_data is not None else []
    sessions = []
    for name, (issue, expected) in SESSIONS.items():
        fixture_name = {"empty_folder": "empty-folder", "unsupported_only_folder": "unsupported-only"}.get(name, name)
        covered = [row["sequence"] for row in rows if row["session"] == fixture_name]
        model_case = "unsupported_finder_shape" if name == "unsupported" else "valid_finder_replay"
        # The existing valid batch exercises file-only incremental replay. Folder
        # fixture coverage cannot inherit its revision or become audit evidence.
        file_model = name in {"create", "duplicate", "rename", "reparent", "delete", "unsupported"}
        evidence = next((record for record in records if record["case"] == model_case), None) if covered and file_model else None
        sessions.append({"session": name, "issue": issue, "expected_disposition": expected,
                         "expected_affected_region": "old_and_new_parents" if name in ("rename", "reparent") else "current_source" if name in ("unsupported", "overflow", "restart", "scan_overlap") else "entry_or_parent_subtree",
                         "fixture_sequences": covered,
                         "model_batch_case": evidence["case"] if evidence else None,
                         "batch_source_revision": evidence["source_revision"] if evidence else None,
                         "last_good_projection": "model_retained" if evidence and name == "unsupported" else "unverified_per_session",
                         "native": "blocked_missing_Finder_and_GPUI_observations",
                         "no_ui_io": "unverified_requires_runtime_observation"})
    return {"schema": "yield.finder-acceptance.v1", "status": "blocked_native_acceptance",
            "build_head": head, "host_os": platform.system(),
            "build_provenance": "operator_declared_log_head_requires_independent_verification",
            "fixture_sha256": hashlib.sha256(fixture).hexdigest(),
            "model_log_sha256": hashlib.sha256(input_data).hexdigest() if input_data is not None else None,
            "model_status": "validated_synthetic_replay_only" if records else "not_captured",
            "model_records": records, "sessions": sessions,
            "native_blockers": ["OPT-1939", "OPT-1940"],
            "capture_command": ["cargo", "test", "--locked", "-p", "yield-source-watcher", "--lib", TEST, "--", "--exact", "--nocapture"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, help="log from the exact macOS model test")
    parser.add_argument("--head", required=True, help="full Git SHA of the model test build")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.head):
        parser.error("head must be a full lowercase Git commit SHA")
    if args.out.exists():
        parser.error("output exists; use a fresh report path")
    try:
        subprocess.run(["git", "cat-file", "-e", args.head + "^{commit}"], cwd=ROOT,
                       capture_output=True, timeout=10, check=True)
        fixture = subprocess.run(["git", "show", args.head + ":" + FIXTURE], cwd=ROOT,
                                 capture_output=True, timeout=10, check=True).stdout
        if len(fixture) > 8192:
            raise ValueError("fixture exceeded byte limit")
        result = report(args.head, fixture, bounded_read(args.input, MAX_INPUT_BYTES) if args.input else None)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("x") as output:
            json.dump(result, output, indent=2, allow_nan=False)
            output.write("\n")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(2, "invalid Finder evidence: %s\n" % error)
    print("blocked_native_acceptance: %s" % args.out)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
