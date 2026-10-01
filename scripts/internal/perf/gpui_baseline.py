#!/usr/bin/env python3
"""Capture the existing CPU waveform benchmark with bounded, reproducible metadata."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
TEST = "browser::waveform::tests::waveform_geometry_cache_benchmark"
PREFIX = "YIELD_BASELINE="
MAX_LOG_BYTES = 1024 * 1024
MAX_RECORD_BYTES = 2048
EXPECTED = {("geometry", 2048), ("gain", 2048), ("gain", 65536),
            ("fade", 2048), ("fade", 65536)}


def command_output(command):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True,
                            timeout=10, check=True)
    if len(result.stdout) > MAX_LOG_BYTES:
        raise ValueError("metadata command exceeded output limit")
    return result.stdout.strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def host_metadata():
    system = platform.system()
    cpu, memory = None, None
    limits = {}
    if system == "Linux":
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                memory = int(line.split()[1]) * 1024
        for name in ("cpu.max", "memory.max"):
            path = Path("/sys/fs/cgroup") / name
            limits[name] = path.read_text().strip() if path.is_file() else None
        limits["affinity_cpu_count"] = len(os.sched_getaffinity(0))
    elif system == "Darwin":
        cpu = command_output(["sysctl", "-n", "machdep.cpu.brand_string"])
        memory = int(command_output(["sysctl", "-n", "hw.memsize"]))
    return {"os": system, "os_release": platform.release(),
            "architecture": platform.machine(), "cpu_model": cpu,
            "logical_cpu_count": os.cpu_count(), "memory_bytes": memory,
            "resource_limits": limits,
            "cpu_geometry_support": "supported" if cpu and memory else "unknown",
            "native_acceptance": "unverified" if system == "Darwin" else "unsupported",
            "native_reason": "This capture does not measure display, input, or audio."}


def parse_records(log):
    """Reject partial, duplicated, stale, or malformed benchmark evidence."""
    def unique_fields(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate benchmark field")
            result[key] = value
        return result

    records = []
    for line in log.splitlines():
        if not line.startswith(PREFIX):
            continue
        if len(line.encode()) > MAX_RECORD_BYTES:
            raise ValueError("benchmark record exceeded byte limit")
        record = json.loads(line[len(PREFIX):], object_pairs_hook=unique_fields)
        fields = {"schema", "kind", "input_columns", "width_px", "height_px",
                  "miss_iterations", "hit_iterations", "miss_total_ns", "hit_total_ns"}
        if not isinstance(record, dict) or set(record) != fields:
            raise ValueError("unexpected benchmark record fields")
        if record["schema"] != "yield.waveform-geometry.v1" or type(record["kind"]) is not str:
            raise ValueError("unsupported benchmark record schema")
        for field in fields - {"schema", "kind"}:
            if type(record[field]) is not int or not 0 < record[field] < 2**64:
                raise ValueError("invalid benchmark numeric field: " + field)
        if (record["kind"], record["input_columns"]) not in EXPECTED:
            raise ValueError("unknown benchmark case")
        geometry = record["kind"] == "geometry"
        if (record["width_px"], record["height_px"], record["miss_iterations"],
                record["hit_iterations"]) != (640 if geometry else 1120, 180,
                                               100 if geometry else 8, 100):
            raise ValueError("benchmark configuration changed; review capture schema")
        records.append(record)
        if len(records) > len(EXPECTED):
            raise ValueError("too many benchmark records")
    if len(records) != len(EXPECTED) or {
        (record["kind"], record["input_columns"]) for record in records
    } != EXPECTED:
        raise ValueError("missing or duplicate benchmark cases; no baseline captured")
    return sorted(records, key=lambda record: (record["kind"], record["input_columns"]))


def run_benchmark(command, timeout):
    # Cargo/test output stays on disk. Timeout kills the process group, including test children.
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=log,
                                   start_new_session=True)
        started = time.monotonic()
        reason = None
        try:
            while process.poll() is None:
                if os.fstat(log.fileno()).st_size > MAX_LOG_BYTES:
                    reason = "benchmark log exceeded byte limit"
                    break
                if time.monotonic() - started >= timeout:
                    reason = "benchmark exceeded timeout"
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
            reason = "benchmark log exceeded byte limit"
        log.seek(0)
        output = log.read(MAX_LOG_BYTES).decode("utf-8", errors="replace")
        if reason:
            raise ValueError(reason)
        if process.returncode != 0:
            raise ValueError("benchmark command failed (exit %s)" % process.returncode)
        return parse_records(output)


def capture(profile, timeout):
    if command_output(["git", "status", "--porcelain", "--untracked-files=normal"]):
        raise ValueError("capture requires a clean checkout, including untracked files")
    # Custom target/profile/flag configuration needs its own reviewed capture schema.
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
        if os.environ.get(name):
            raise ValueError("unsupported build override: " + name)
    if any(name.startswith("CARGO_PROFILE_") or name.startswith("CARGO_TARGET_")
           for name in os.environ):
        raise ValueError("unsupported Cargo profile/target override")
    head = command_output(["git", "rev-parse", "HEAD"])
    fixture = {"kind": "constant_bands", "band_values": [0.5, 0.4, 0.3, 0.6],
               "input_columns": [2048, 65536], "seed": None,
               "source_sha256": digest(ROOT / "crates/yield-ui/src/browser/waveform.rs")}
    files = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]
    config = {name: digest(ROOT / name) for name in files if (ROOT / name).is_file()}
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    for name in ("config", "config.toml"):
        if (cargo_home / name).is_file():
            config["cargo_home/" + name] = digest(cargo_home / name)
    command = ["cargo", "test", "--locked", "-p", "yield-ui", "--lib", "--profile",
               profile, TEST, "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"]
    manifest = {"schema": "yield.gpui-baseline.v1", "status": "blocked",
                "build": {"head": head, "tree": command_output(["git", "rev-parse", "HEAD^{tree}"]),
                          "rustc": command_output(["rustc", "-Vv"]),
                          "cargo": command_output(["cargo", "-V"]),
                          "profile": profile, "config_sha256": config,
                          "build_jobs": os.environ.get("CARGO_BUILD_JOBS")},
                "host": host_metadata(), "fixture": fixture,
                "measurement": {"command": command, "timeout_seconds": timeout,
                                "scope": "CPU waveform geometry; excludes compilation, painting, GPU, input and audio",
                                "threshold": None}, "results": []}
    if platform.system() not in ("Linux", "Darwin") or manifest["host"]["cpu_geometry_support"] != "supported":
        manifest["blocker"] = "unidentified or unsupported CPU geometry host"
        return manifest
    try:
        manifest["results"] = run_benchmark(command, timeout)
        if command_output(["git", "rev-parse", "HEAD"]) != head or command_output(
            ["git", "status", "--porcelain", "--untracked-files=normal"]
        ):
            raise ValueError("checkout changed during capture")
        manifest["status"] = "captured"
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        manifest["results"] = []
        manifest["blocker"] = str(error)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--profile", choices=("dev", "release-local"), default="dev")
    parser.add_argument("--timeout", type=float, default=600)
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or not 1 <= args.timeout <= 1800:
        parser.error("timeout must be finite and between 1 and 1800 seconds")
    if args.out.exists():
        parser.error("output already exists; use a new capture path")
    try:
        manifest = capture(args.profile, args.timeout)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, "baseline blocked: %s\n" % error)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("x") as output:
        json.dump(manifest, output, indent=2, allow_nan=False)
        output.write("\n")
    print("%s: %s" % (manifest["status"], args.out))
    return 0 if manifest["status"] == "captured" else 1


if __name__ == "__main__":
    raise SystemExit(main())
