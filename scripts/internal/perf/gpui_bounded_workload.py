#!/usr/bin/env python3
"""Capture bounded owner cancellation and CPU cache growth; never native acceptance."""

import argparse
import json
from pathlib import Path

from gpui_baseline import ROOT, command_output, digest, host_metadata, run_benchmark

CASES = [
    ("yield-library", "file_operation_owner::tests::bounded_queue_cancellation_workload"),
    ("yield-ui", "browser::waveform::tests::bounded_waveform_cache_workload"),
]
EXPECTED = ({("file-owner", cycle, window) for cycle in range(8) for window in range(2)}
            | {("file-owner-retired", cycle) for cycle in range(8)}
            | {("waveform-cache", generation) for generation in range(32)})


def parse_records(log: str) -> list[dict]:
    def unique_fields(pairs):
        result = dict(pairs)
        if len(result) != len(pairs):
            raise ValueError("duplicate workload field")
        return result

    rows = []
    for line in log.splitlines():
        if line.startswith("YIELD_WORKLOAD="):
            if len(line) > 2048:
                raise ValueError("oversized workload record")
            row = json.loads(line.removeprefix("YIELD_WORKLOAD="), object_pairs_hook=unique_fields)
            if any(not isinstance(value, int) or isinstance(value, bool) or value < 0
                   for key, value in row.items() if key != "kind"):
                raise ValueError("workload measurements must be nonnegative integers")
            rows.append(row)
    return rows


def validate(rows: list[dict]) -> None:
    keys = []
    for row in rows:
        kind = row["kind"]
        if kind == "file-owner":
            keys.append((kind, row["cycle"], row["window"]))
            if (row["effects_executed"] or row["result_handles_after_drain"]
                    or row["retained_rows"] != (33 if row["window"] == 0 else 64)
                    or row["cancelled"] != (33 if row["window"] == 0 else 31)
                    or not 0 < row["input_peak_bytes"] <= 4 * 1024 * 1024
                    or row["input_after_drain_bytes"] != row["input_peak_bytes"]):
                raise ValueError("unexpected owner cancellation/retention result")
        elif kind == "file-owner-retired":
            keys.append((kind, row["cycle"]))
            if row["input_after_custody_bytes"] or row["retained_after_custody_rows"]:
                raise ValueError("custody did not clear owner accounting")
        elif kind == "waveform-cache":
            keys.append((kind, row["generation"]))
            if any(row[field] != expected for field, expected in (
                ("inputs", 65536), ("width_px", 1120), ("hits_per_generation", 15),
                ("base_slots", 1), ("preview_slots", 1), ("base_paths", 4),
                ("preview_paths", 4), ("external_path_refs", 0)
            )):
                raise ValueError("unexpected cache growth configuration/result")
        else:
            raise ValueError("unknown workload kind")
    if len(keys) != len(set(keys)) or set(keys) != EXPECTED:
        raise ValueError("missing/duplicate workload windows; capture incomplete")


def capture() -> dict:
    if command_output(["git", "status", "--porcelain", "--untracked-files=normal"]):
        raise ValueError("capture requires a clean checkout")
    head = command_output(["git", "rev-parse", "HEAD"])
    result = {"schema": "yield.bounded-workload.v1", "issue": "OPT-1736",
              "head": head, "host": host_metadata(),
              "toolchain": command_output(["rustc", "-Vv"]),
              "fixture_hashes": {path: digest(ROOT / path) for path in (
                  "crates/yield-library/src/file_operation_owner.rs",
                  "crates/yield-ui/src/browser/waveform.rs", "Cargo.lock")},
              "scope": "Production queue/accounting/cancellation with synthetic authorization and test scheduling barriers; CPU waveform cache. No physical effects, GPUI frame/input/audio, process RSS or allocator byte measurement.",
              "native_follow_up": "OPT-1983", "commands": [], "records": []}
    for package, test in CASES:
        command = ["cargo", "test", "--locked", "-p", package, "--lib", test,
                   "--", "--exact", "--ignored", "--nocapture"]
        result["commands"].append({"argv": command, "timeout_seconds": 60})
        result["records"].extend(run_benchmark(command, 60, parser=parse_records))
    validate(result["records"])
    if command_output(["git", "rev-parse", "HEAD"]) != head or command_output(
        ["git", "status", "--porcelain", "--untracked-files=normal"]
    ):
        raise ValueError("checkout changed during capture")
    result["status"] = "captured"
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.out.exists():
        parser.error("output already exists; use a new capture path")
    result = capture()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("x") as output:
        json.dump(result, output, indent=2)
        output.write("\n")
    print(f"captured {len(result['records'])} windows: {args.out}")
