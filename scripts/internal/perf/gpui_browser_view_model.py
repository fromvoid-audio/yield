#!/usr/bin/env python3
"""Capture the 20k BrowserViewModel CPU workload; this does not measure GPUI frames."""

import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
TEST = "tests::browser_view_model_20k_performance_workload"
PREFIX = "YIELD_BROWSER_PERF="
MAX_LOG_BYTES = 1024 * 1024
MAX_RECORD_BYTES = 16 * 1024


def command_output(command):
    return subprocess.run(command, cwd=ROOT, capture_output=True, text=True,
                          timeout=15, check=True).stdout.strip()


def digest_bytes(value):
    return hashlib.sha256(value).hexdigest()


def digest(path):
    return digest_bytes(path.read_bytes())


def host_metadata():
    system = platform.system()
    cpu = None
    memory = None
    if system == "Darwin":
        cpu = command_output(["sysctl", "-n", "machdep.cpu.brand_string"])
        memory = int(command_output(["sysctl", "-n", "hw.memsize"]))
    elif system == "Linux":
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                memory = int(line.split()[1]) * 1024
    return {"os": system, "os_release": platform.release(),
            "architecture": platform.machine(), "cpu_model": cpu,
            "logical_cpu_count": os.cpu_count(), "memory_bytes": memory}


def unique_fields(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate benchmark field")
        result[key] = value
    return result


def parse_record(output):
    records = []
    for line in output.splitlines():
        offset = line.find(PREFIX)
        if offset >= 0:
            if len(line.encode()) > MAX_RECORD_BYTES:
                raise ValueError("oversized browser workload record")
            records.append(json.loads(line[offset + len(PREFIX):], object_pairs_hook=unique_fields))
    if len(records) != 1:
        raise ValueError("expected one complete browser workload record")
    row = records[0]
    expected = {"schema", "entries", "iterations", "query_ns", "sort_ns",
                "collection_counts_ns", "collection_counts"}
    validate_record(row, expected)
    return row


def validate_record(row, expected=None):
    expected = expected or {"schema", "entries", "iterations", "query_ns", "sort_ns",
                            "collection_counts_ns", "collection_counts"}
    if not isinstance(row, dict) or set(row) != expected or row["schema"] != "yield.browser-vm.v1":
        raise ValueError("unexpected browser workload schema")
    if (row["entries"], row["iterations"]) != (20_000, 30):
        raise ValueError("browser workload fixture changed; review capture schema")
    for key in ("query_ns", "sort_ns", "collection_counts_ns"):
        values = row[key]
        if (not isinstance(values, list) or len(values) != row["iterations"]
                or any(type(value) is not int or value <= 0 for value in values)):
            raise ValueError("invalid timing sample list: " + key)
    if row["collection_counts"] != [3334, 3334, 3333, 3333, 3333, 3333]:
        raise ValueError("collection-count correctness check changed")
    return row


def run(command, timeout, env):
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=log, stderr=log,
                                   start_new_session=True)
        started = time.monotonic()
        reason = None
        try:
            while process.poll() is None:
                if os.fstat(log.fileno()).st_size > MAX_LOG_BYTES:
                    reason = "browser workload log exceeded size limit"
                    break
                if time.monotonic() - started >= timeout:
                    reason = "browser workload exceeded timeout"
                    break
                time.sleep(0.05)
        finally:
            if process.poll() is None or reason:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
        if os.fstat(log.fileno()).st_size > MAX_LOG_BYTES:
            reason = "browser workload log exceeded size limit"
        log.seek(0)
        output = log.read(MAX_LOG_BYTES).decode("utf-8", errors="replace")
        if reason:
            raise ValueError(reason)
        if process.returncode != 0:
            raise ValueError("browser workload command failed (exit %s)" % process.returncode)
        return parse_record(output)


def summarize(runs):
    stages = ("query_ns", "sort_ns", "collection_counts_ns")
    summary = {}
    for stage in stages:
        values = [sample for run_result in runs for sample in run_result[stage]]
        ordered = sorted(values)
        p95 = ordered[math.ceil(0.95 * len(ordered)) - 1]
        run_p95 = []
        for run_result in runs:
            samples = sorted(run_result[stage])
            run_p95.append(samples[math.ceil(0.95 * len(samples)) - 1])
        summary[stage.removesuffix("_ns")] = {
            "samples": len(values), "median_ns": int(statistics.median(values)),
            "p95_ns": p95, "max_ns": max(values),
            "per_run_p95_ns": run_p95,
            "calibrated_limit_ns": max(run_p95)}
    return summary


def compare_calibration(candidate, baseline, baseline_path):
    if baseline.get("schema") != "yield.gpui-browser-view-model.v1":
        raise ValueError("unsupported calibration capture schema")
    if baseline.get("status") != "captured_uncalibrated":
        raise ValueError("calibration input must be an uncalibrated pre-change capture")
    if baseline.get("host") != candidate["host"]:
        raise ValueError("calibrated PASS refused: host identity differs from baseline")
    if baseline.get("build") != candidate["build"]:
        raise ValueError("calibrated PASS refused: build configuration differs")
    if baseline.get("dataset") != candidate["dataset"]:
        raise ValueError("calibrated PASS refused: fixture identity differs from baseline")
    baseline_source = baseline.get("source", {})
    baseline_runner = baseline_source.get("tracked_file_sha256", {}).get(
        "scripts/internal/perf/gpui_browser_view_model.py")
    if baseline_runner != candidate["source"]["tracked_file_sha256"].get(
            "scripts/internal/perf/gpui_browser_view_model.py"):
        raise ValueError("calibrated PASS refused: benchmark runner differs from baseline")
    ancestor = subprocess.run(["git", "merge-base", "--is-ancestor",
                               baseline_source.get("head", ""), candidate["source"]["head"]],
                              cwd=ROOT, check=False)
    if ancestor.returncode != 0:
        raise ValueError("calibrated PASS refused: baseline commit is not an ancestor")
    baseline_runs = baseline.get("runs")
    candidate_runs = candidate.get("runs")
    if not isinstance(baseline_runs, list) or len(baseline_runs) != 5:
        raise ValueError("calibration input must contain exactly five independent runs")
    if not isinstance(candidate_runs, list) or len(candidate_runs) != 5:
        raise ValueError("calibrated comparison requires exactly five candidate runs")
    for row in baseline_runs + candidate_runs:
        validate_record(row)
    limits = {}
    for stage in ("query", "sort", "collection_counts"):
        per_run = [sorted(row[stage + "_ns"])[math.ceil(0.95 * 30) - 1]
                   for row in baseline_runs]
        center = statistics.median(per_run)
        mad = statistics.median(abs(value - center) for value in per_run)
        # Add only the observed run-to-run spread or a 0.25 ms clock/scheduler-noise floor.
        limits[stage] = max(per_run) + max(250_000, int(3 * mad))
    observed = {stage: candidate["summary"][stage]["p95_ns"] for stage in limits}
    metadata = {
        "kind": "same_host_regression_guard",
        "source_capture_sha256": digest(baseline_path),
        "rule": "candidate aggregate p95 must stay within the repeated baseline worst per-run p95 plus max(0.25 ms, 3x median absolute run spread)",
        "limits_ns": limits,
        "observed_p95_ns": observed}
    status = "passed" if all(observed[stage] <= limits[stage] for stage in limits) else "failed"
    return metadata, status


def source_identity():
    status = command_output(["git", "status", "--porcelain", "--untracked-files=all"])
    diff = subprocess.run(["git", "diff", "--binary", "HEAD", "--"], cwd=ROOT,
                          capture_output=True, check=True).stdout
    tracked = ["crates/yield-core/src/lib.rs", "scripts/internal/perf/gpui_browser_view_model.py",
               "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]
    files = {name: digest(ROOT / name) for name in tracked if (ROOT / name).is_file()}
    return {"head": command_output(["git", "rev-parse", "HEAD"]),
            "tree": command_output(["git", "rev-parse", "HEAD^{tree}"],),
            "worktree_status": status, "tracked_file_sha256": files,
            "diff_from_head_sha256": digest_bytes(diff)}


def workload_source_digest():
    source = (ROOT / "crates/yield-core/src/lib.rs").read_text(encoding="utf-8")
    marker = "    /// Opt-in 20k browser CPU workload."
    start = source.find(marker)
    end = source.find("\n    #[test]\n", start)
    if start < 0 or end < 0:
        raise ValueError("cannot identify the bounded browser fixture source")
    return digest_bytes(source[start:end].encode("utf-8"))


def capture(profile, repetitions, timeout, baseline_path):
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
        if os.environ.get(name):
            raise ValueError("unsupported build override: " + name)
    if any(name.startswith("CARGO_PROFILE_") or name.startswith("CARGO_TARGET_")
           for name in os.environ):
        raise ValueError("unsupported Cargo profile/target override")
    source_before = source_identity()
    command = ["cargo", "test", "--locked", "-p", "yield-core", "--lib", "--profile",
               profile, TEST, "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"]
    rustc = command_output(["rustc", "-Vv"])
    rust_identity = "-".join(line.split(": ", 1)[1] for line in rustc.splitlines()
                              if line.startswith(("release: ", "host: ")))
    lock_hash = digest(ROOT / "Cargo.lock")[:12]
    common_dir = Path(command_output(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"]))
    validation_root = common_dir.parent.parent / "yield-native-target" / "validation"
    cargo_target = validation_root / f"{rust_identity}-{lock_hash}"
    cargo_target.mkdir(parents=True, exist_ok=True)
    cargo_env = os.environ.copy()
    cargo_env["CARGO_TARGET_DIR"] = str(cargo_target)
    cargo_env["CARGO_BUILD_JOBS"] = "2"
    cargo_env["CARGO_INCREMENTAL"] = "0"
    cargo_env["YIELD_GPUI_CI_CHECKOUT"] = str(ROOT)
    result = {"schema": "yield.gpui-browser-view-model.v1", "status": "blocked",
              "source": source_before, "host": host_metadata(),
              "build": {"rustc": rustc,
                        "cargo": command_output(["cargo", "-V"]), "profile": profile,
                        "cargo_home_config_sha256": {},
                        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
                        "workspace_config_sha256": {
                            name: digest(ROOT / name)
                            for name in ("Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml")
                            if (ROOT / name).is_file()},
                        "target_dir": str(cargo_target), "build_jobs": 2,
                        "incremental": False},
              "dataset": {"kind": "deterministic-synthetic-browser-catalog",
                          "entries": 20000, "needle_matches": 200,
                          "curation_membership": "one-of-six-round-robin",
                          "fixture_version": "browser-vm-20k-v1",
                          "workload_source_sha256": workload_source_digest()},
              "measurement": {"scope": "BrowserViewModel query projection, sort projection, and collection counts on CPU; excludes UI selection reconciliation, GPUI rendering, display submission, GPU completion, input delivery, and audio",
                              "command": command, "timeout_seconds_per_run": timeout,
                              "iterations_per_stage_per_run": 30,
                              "repetitions": repetitions, "coordinator": "shared .git/local-ci/active.lock",
                              "thresholds": None},
              "runs": [], "summary": {}}
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    for name in ("config", "config.toml"):
        path = cargo_home / name
        if path.is_file():
            result["build"]["cargo_home_config_sha256"][name] = digest(path)
    if not result["host"]["cpu_model"] or not result["host"]["memory_bytes"]:
        result["blocker"] = "host model or memory could not be identified"
        return result
    state_dir = common_dir / "local-ci"
    state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    lock_path = state_dir / "active.lock"
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        info = os.fstat(fd)
        entry = lock_path.lstat()
        if (not lock_path.is_file() or info.st_uid != os.geteuid() or info.st_nlink != 1
                or (info.st_dev, info.st_ino) != (entry.st_dev, entry.st_ino)):
            raise ValueError("shared local-CI coordinator lock failed ownership checks")
        fcntl.flock(fd, fcntl.LOCK_EX)
        for _ in range(repetitions):
            result["runs"].append(run(command, timeout, cargo_env))
    finally:
        os.close(fd)
    source_after = source_identity()
    if source_after != source_before:
        raise ValueError("source identity changed during workload capture")
    result["summary"] = summarize(result["runs"])
    if baseline_path:
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        result["measurement"]["thresholds"], result["status"] = compare_calibration(
            result, baseline, baseline_path)
    else:
        result["status"] = "captured_uncalibrated"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--baseline", type=Path,
                        help="same-host pre-change capture used for scoped CPU regression limits")
    parser.add_argument("--profile", choices=("dev", "release-local"), default="release-local")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=300)
    args = parser.parse_args()
    if args.out.exists():
        parser.error("output already exists; choose a fresh path")
    if not 1 <= args.repetitions <= 10:
        parser.error("repetitions must be between 1 and 10")
    if not math.isfinite(args.timeout) or not 1 <= args.timeout <= 900:
        parser.error("timeout must be finite and between 1 and 900 seconds")
    try:
        manifest = capture(args.profile, args.repetitions, args.timeout, args.baseline)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, "browser workload blocked: %s\n" % error)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("x") as output:
        json.dump(manifest, output, indent=2, allow_nan=False)
        output.write("\n")
    print("%s: %s" % (manifest["status"], args.out))
    return 0 if manifest["status"] in {"captured_uncalibrated", "passed"} else 1


if __name__ == "__main__":
    raise SystemExit(main())
