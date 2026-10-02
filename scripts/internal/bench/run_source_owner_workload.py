#!/usr/bin/env python3
"""Run bounded actual source-owner measurements; never infer a durability SLO."""

import argparse
import datetime
import json
import os
import platform
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
MARKER = "SOURCE_OWNER_WORKLOAD_JSON="
COMMAND = ["cargo", "test", "--locked", "-p", "yield-library", "--test",
           "source_owner_workload", "--", "--ignored", "--nocapture"]


def capture(command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runs", type=int, choices=range(2, 6), default=2,
                        help="2-5 independent isolated runs (default 2)")
    args = parser.parse_args()
    head = capture(["git", "rev-parse", "HEAD"])
    dirty = capture(["git", "status", "--porcelain"])
    host = {"system": platform.system(), "release": platform.release(),
            "machine": platform.machine(), "logical_cpus": os.cpu_count(),
            "cpu_affinity_count": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "rustc": capture(["rustc", "--version"]),
            "sqlite_mode": "bundled", "build_profile": "cargo-test",
            "profile_mode": "isolated-temporary-owned-profile",
            "cpu_quota": "unmeasured", "storage_device": "unqualified"}
    evidence = []
    stable = None
    for index in range(args.runs):
        print(f"source-owner workload run {index + 1}/{args.runs}", flush=True)
        result = subprocess.run(COMMAND, cwd=ROOT, text=True, capture_output=True, timeout=300)
        if result.returncode:
            raise RuntimeError(f"workload failed ({result.returncode})\n{result.stdout}\n{result.stderr}")
        lines = [line.split(MARKER, 1)[1] for line in result.stdout.splitlines() if MARKER in line]
        if len(lines) != 1:
            raise RuntimeError("expected exactly one owner evidence record")
        sample = json.loads(lines[0])
        identity = {key: sample[key] for key in ("schema_version", "fixture", "rows", "batch_size",
                    "page_size", "fixture_blake3", "initial_revision", "final_revision",
                    "reopened_revision", "restart_rows_match", "old_lifecycle_stale")}
        if stable is not None and identity != stable:
            raise RuntimeError("independent workload results are not reproducible")
        stable = identity
        evidence.append(sample)
    if capture(["git", "rev-parse", "HEAD"]) != head or capture(["git", "status", "--porcelain"]) != dirty:
        raise RuntimeError("source state changed during workload; rerun on a fixed source state")
    bundle = {"schema_version": 1, "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "head": head, "dirty": bool(dirty), "dirty_paths": dirty.splitlines(), "command": COMMAND,
              "host": host, "reproducible_identity": stable, "runs": evidence,
              "qualification": "portable-metadata-owner-measurement-only",
              "unmeasured_components": ["physical filesystem publication", "physical content hashing",
                   "journal capacity admission", "non-snapshot queue wait", "independent SQL/checkpoint time",
                   "power-loss/crash recovery", "native GPUI latency", "SLO thresholds"]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(bundle, indent=2) + "\n", encoding="utf-8")
    print(f"PASS: {args.runs} reproducible runs; evidence: {args.output}")


if __name__ == "__main__":
    main()
