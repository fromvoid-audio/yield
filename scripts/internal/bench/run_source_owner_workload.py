#!/usr/bin/env python3
"""Run bounded actual source-owner measurements; never infer a durability SLO."""

import argparse
import datetime
import hashlib
import json
import os
import platform
from pathlib import Path
import selectors
import signal
import subprocess
import time
import tempfile

ROOT = Path(__file__).resolve().parents[3]
MARKER = "SOURCE_OWNER_WORKLOAD_JSON="
COMMAND = ["cargo", "test", "--locked", "-p", "yield-library", "--test",
           "source_owner_workload", "--", "--ignored", "--nocapture"]
MAX_OUTPUT = 8 * 1024 * 1024
U64_MAX = (1 << 64) - 1
FIXTURE = {"schema_version": 2, "fixture": "source-owner-metadata-v1", "rows": 4096,
           "batch_size": 64, "page_size": 64,
           "fixture_blake3": "fc2684b6fbed3f36f42f29f5b572ea5288c464ef95bfb806e13a556e02ad523a",
           "initial_revision": 0, "final_revision": 65, "reopened_revision": 65,
           "restart_rows_match": True, "old_lifecycle_stale": True}
CURATION = {"selected_rows": 64, "expected_revision": 65, "committed_revision": 66,
            "reopened_revision": 66, "exact_replay": True, "conflict_rejected": True,
            "stale_rejected": True, "historical_receipt_equal": True,
            "superseded_request_rejected": True, "selected_metadata_persisted": True,
            "first_unselected_unchanged": True}
CURATION_TIMINGS = {"commit_ns", "exact_replay_ns", "conflict_rejection_ns",
                    "stale_rejection_ns", "close_ns", "reopen_ns",
                    "historical_observation_ns", "superseded_rejection_ns",
                    "persisted_selection_ns"}
BUILD_OVERRIDES = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
                   "CARGO_BUILD_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER",
                   "RUSTC_WORKSPACE_WRAPPER", "RUSTUP_TOOLCHAIN", "RUSTC_BOOTSTRAP",
                   "CARGO_RUSTC_CODEGEN_BACKEND"}


def run_bounded(command, timeout=300, output_limit=MAX_OUTPUT):
    """Limit combined output and elapsed time; reap this command's own process group."""
    if os.name != "posix":
        raise RuntimeError("this bounded process runner requires a POSIX host")
    started = time.monotonic()
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, start_new_session=True) as process:
        chunks = {"stdout": bytearray(), "stderr": bytearray()}
        size = 0
        completed = False
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ, "stdout")
                selector.register(process.stderr, selectors.EVENT_READ, "stderr")
                while selector.get_map():
                    remaining = timeout - (time.monotonic() - started)
                    if remaining <= 0:
                        raise RuntimeError("workload process exceeded its time budget")
                    for key, _ in selector.select(min(remaining, 1)):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            continue
                        size += len(data)
                        if size > output_limit:
                            raise RuntimeError("workload process exceeded its output budget")
                        chunks[key.data].extend(data)
                remaining = timeout - (time.monotonic() - started)
                if remaining <= 0:
                    raise RuntimeError("workload process exceeded its time budget")
                returncode = process.wait(timeout=remaining)
                completed = True
        finally:
            # Only the dedicated group created above; never shared Cargo or other chats.
            if not completed:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
        return subprocess.CompletedProcess(command, returncode,
            chunks["stdout"].decode("utf-8", errors="strict"),
            chunks["stderr"].decode("utf-8", errors="strict"))


def capture(command):
    result = run_bounded(command, timeout=30, output_limit=1024 * 1024)
    if result.returncode:
        raise RuntimeError(f"metadata command failed: {command[0]}")
    return result.stdout.strip()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def integer(value):
    require(type(value) is int and 0 <= value <= U64_MAX, "expected finite u64 measurement")
    return value


def exact_fields(sample, expected):
    for key, value in expected.items():
        require(key in sample and type(sample[key]) is type(value) and sample[key] == value,
                f"fixture/receipt assertion mismatch: {key}")


def fields(sample, expected):
    require(type(sample) is dict and set(sample) == set(expected), "unexpected or missing evidence fields")


def distribution(sample, count):
    fields(sample, ("unit", "count", "samples", "p50", "p95", "max"))
    require(sample["unit"] == "ns" and integer(sample["count"]) == count,
            "wrong distribution boundary/count")
    values = sample["samples"]
    require(type(values) is list and len(values) == count, "partial timing samples")
    ordered = sorted(integer(value) for value in values)
    exact_fields(sample, {"p50": ordered[(count - 1) // 2],
        "p95": ordered[(count * 95 + 99) // 100 - 1], "max": ordered[-1]})


def storage(sample):
    require(sample["capacity_claim"] is False, "storage cannot supply a capacity claim")
    if sample["status"] == "measured":
        fields(sample, ("status", "capacity_claim", "main_retained_vfs_bytes", "wal_retained_vfs_bytes",
                       "shm_root_observed_bytes", "logical_page_count", "page_size"))
        for key in ("main_retained_vfs_bytes", "wal_retained_vfs_bytes",
                    "shm_root_observed_bytes", "logical_page_count", "page_size"):
            integer(sample[key])
        require(sample["logical_page_count"] > 0 and sample["page_size"] in
                (512, 1024, 2048, 4096, 8192, 16384, 32768, 65536), "invalid SQLite storage")
    else:
        fields(sample, ("status", "capacity_claim", "reason"))
        exact_fields(sample, {"status": "unavailable",
            "reason": "retained-storage-observation-unavailable"})


def queue(sample, count):
    fields(sample, ("attempts", "accepted", "full", "disconnected", "pending_dispatch",
                   "dequeued", "peak_pending_dispatch", "dispatch_wait_buckets",
                   "max_dispatch_wait_micros", "dispatch_wait_micros"))
    exact_fields(sample, {"attempts": count, "accepted": count, "full": 0,
        "disconnected": 0, "pending_dispatch": 0, "dequeued": count,
        "peak_pending_dispatch": 1})
    buckets = sample["dispatch_wait_buckets"]
    require(type(buckets) is list and len(buckets) == 8 and
            sum(integer(value) for value in buckets) == count, "partial dispatch histogram")
    require(integer(sample["max_dispatch_wait_micros"]) <= integer(sample["dispatch_wait_micros"]),
            "inconsistent dispatch maximum")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON field")
        result[key] = value
    return result


def parse_evidence(result):
    require(result.returncode == 0, "workload process failed; no success evidence")
    require(result.stdout.count(MARKER) == 1, "expected exactly one complete owner evidence record")
    lines = [line.split(MARKER, 1)[1] for line in result.stdout.splitlines() if MARKER in line]
    sample = json.loads(lines[0], object_pairs_hook=unique_object)
    fields(sample, set(FIXTURE) | {"curation", "timings", "storage", "page_queue", "reopened_page_queue", "limits"})
    exact_fields(sample, FIXTURE)
    fields(sample["curation"], set(CURATION) | {"timings", "storage_after_commit"})
    exact_fields(sample["curation"], CURATION)
    timings = sample["timings"]
    fields(timings, ("open_ns", "audit_ns", "close_ns", "reopen_ns", "manifest_batches", "pages", "reopened_pages"))
    for key in ("open_ns", "audit_ns", "close_ns", "reopen_ns"):
        integer(timings[key])
    for key, count in (("manifest_batches", 64), ("pages", 65), ("reopened_pages", 65)):
        distribution(timings[key], count)
    require(set(sample["curation"]["timings"]) == CURATION_TIMINGS,
            "missing or mislabeled curation boundary")
    for value in sample["curation"]["timings"].values():
        integer(value)
    storage(sample["storage"])
    storage(sample["curation"]["storage_after_commit"])
    queue(sample["page_queue"], 65)
    queue(sample["reopened_page_queue"], 66)
    limits = sample["limits"]
    require(type(limits) is list and 1 <= len(limits) <= 16 and
            all(type(value) is str and 1 <= len(value) <= 256 for value in limits),
            "missing finite qualification limits")
    return sample


def build_configuration(environ=os.environ):
    overrides = sorted(key for key in environ if key in BUILD_OVERRIDES or
                       key.startswith("CARGO_PROFILE_") or
                       (key.startswith("CARGO_TARGET_") and key != "CARGO_TARGET_DIR"))
    require(not overrides, f"unrecorded build semantic overrides: {', '.join(overrides)}")
    files = {name: ROOT / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
             "rust-toolchain", ".cargo/config.toml", ".cargo/config", "crates/yield-library/Cargo.toml")}
    cargo_home = Path(environ.get("CARGO_HOME", Path.home() / ".cargo"))
    files.update({"cargo_home_config": cargo_home / "config",
                  "cargo_home_config_toml": cargo_home / "config.toml"})
    hashes = {}
    for name, path in files.items():
        if path.exists():
            require(path.stat().st_size <= 1024 * 1024, "build configuration exceeds budget")
            hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()
        else:
            hashes[name] = None
    return hashes


def checkout():
    require(not capture(["git", "status", "--porcelain"]), "workload requires a clean checkout")
    return capture(["git", "rev-parse", "HEAD"])


def collect(runs):
    require(type(runs) is int and 2 <= runs <= 5, "expected 2-5 independent runs")
    head = checkout()
    configuration = build_configuration()
    host = {"system": platform.system(), "release": platform.release(),
            "machine": platform.machine(), "logical_cpus": os.cpu_count(),
            "cpu_affinity_count": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "rustc": capture(["rustc", "--version"]), "cargo": capture(["cargo", "--version"]),
            "sqlite_mode": "bundled", "build_profile": "cargo-test",
            "profile_mode": "isolated-temporary-owned-profile",
            "cpu_quota": "unmeasured", "storage_device": "unqualified"}
    evidence = []
    for index in range(runs):
        require(checkout() == head and build_configuration() == configuration,
                "source/build state changed during workload")
        print(f"source-owner workload run {index + 1}/{runs}", flush=True)
        evidence.append(parse_evidence(run_bounded(COMMAND)))
        require(checkout() == head and build_configuration() == configuration,
                "source/build state changed during workload")
    return {"schema_version": 2, "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "head": head, "dirty": False, "command": COMMAND, "host": host,
            "configuration_sha256": configuration, "reproducible_identity": FIXTURE,
            "reproducible_curation": CURATION, "runs": evidence,
            "qualification": "portable-metadata-owner-measurement-only",
            "unmeasured_components": ["physical filesystem publication", "physical content hashing",
                "journal capacity admission", "non-snapshot queue wait", "independent SQL/checkpoint time",
                "power-loss/crash recovery", "native GPUI latency", "SLO thresholds"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runs", type=int, choices=range(2, 6), default=2,
                        help="2-5 independent isolated runs (default 2)")
    args = parser.parse_args()
    bundle = collect(args.runs)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=args.output.parent,
                                         prefix=".source-owner-evidence-", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(json.dumps(bundle, indent=2) + "\n")
        temporary.replace(args.output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    print(f"PASS: {args.runs} reproducible runs; evidence: {args.output}")


if __name__ == "__main__":
    main()
