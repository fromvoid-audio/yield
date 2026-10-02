#!/usr/bin/env python3
"""Collect one actual global SQLite process-death observation, never qualification."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile

# Reuse the existing bounded POSIX subprocess/configuration implementation. It
# kills/reaps only its dedicated process group, including the fixture child.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "internal" / "bench"))
import run_source_owner_workload as runtime

ROOT = Path(__file__).resolve().parents[2]
TEST = "committed_registry_receipt_survives_process_death_and_refenced_replay"
TARGET = "global_registry_participant_process"
FIXTURE = "crates/yield-library/tests/" + TARGET + ".rs"
PREFIX = "YIELD_GLOBAL_RECOVERY_CASE="
BUILD = ["cargo", "test", "--locked", "-p", "yield-library", "--no-default-features",
         "--test", TARGET, "--no-run", "--message-format=json"]
RUN = ["<compiled-test>", "--exact", TEST, "--nocapture", "--test-threads=1"]
BUILD_LIMIT = 4 * 1024 * 1024
LOG_LIMIT = 256 * 1024
REPORT_LIMIT = 16 * 1024
RECORD_LIMIT = 2048
BINARY_LIMIT = 256 * 1024 * 1024
UNEXERCISED = ["physical_filesystem_publication", "power_loss_and_media_durability",
               "journal_admission_and_recovery", "source_receipt_recovery",
               "harvest_recovery", "macos_native", "windows_native", "durability_slo"]
CASE = {"schema": "yield.global-recovery-case.v1",
        "case": "committed_registry_receipt_process_death",
        "provenance": "actual_normal_build_owned_process_sqlite",
        "outcome": "observed_metadata_replay",
        "observations": ["committed_receipt_survived_owned_process_death",
            "competing_profile_ownership_denied", "old_acquisition_rejected",
            "refenced_replay_preserved_historical_receipt", "changed_payload_and_work_rejected",
            "later_registry_edit_preserved", "one_receipt_retained"]}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate field")
        result[key] = value
    return result


def decode(data, limit):
    require(type(data) is bytes and len(data) <= limit, "input byte limit")
    try:
        return json.loads(data, object_pairs_hook=unique,
                          parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))
    except (UnicodeError, RecursionError, json.JSONDecodeError) as error:
        raise ValueError("malformed JSON") from error


def shape(value, fields):
    require(type(value) is dict and set(value) == set(fields), "unknown or missing fields")


def exact(value, expected):
    require(type(value) is type(expected) and value == expected, "unexpected fixed observation")


def digest(value, length=64):
    require(type(value) is str and re.fullmatch(r"[0-9a-f]{%d}" % length, value), "invalid digest")


def parse_case(stdout, stderr, status):
    require(type(status) is int and status == 0, "failed process cannot yield success")
    require(len(stdout) + len(stderr) <= LOG_LIMIT, "run log byte limit")
    require(stdout.count(PREFIX.encode()) == 1 and PREFIX.encode() not in stderr,
            "missing or ambiguous case marker")
    records = [line[len(PREFIX):] for line in stdout.decode("utf-8").splitlines()
               if line.startswith(PREFIX)]
    require(len(records) == 1, "invalid marker boundary")
    result = decode(records[0].encode(), RECORD_LIMIT)
    shape(result, CASE)
    for key, value in CASE.items():
        exact(result[key], value)
    return result


def source_binding():
    runtime.ROOT = ROOT
    head = runtime.checkout()
    tree = runtime.capture(["git", "rev-parse", "HEAD^{tree}"])
    digest(head, 40)
    digest(tree, 40)
    configuration = runtime.build_configuration()
    # No paths or config contents enter the public record, only fixed names/digests.
    return {"head": head, "tree": tree, "dirty": False,
            "configuration_sha256": configuration}


def artifact(stdout):
    require(len(stdout) <= BUILD_LIMIT, "build log byte limit")
    candidates = []
    for line in stdout.splitlines():
        entry = decode(line, BUILD_LIMIT)
        if entry.get("reason") != "compiler-artifact":
            continue
        target = entry.get("target", {})
        if target.get("name") != TARGET:
            continue
        require(target.get("kind") == ["test"] and entry.get("features") == []
                and entry.get("profile", {}).get("test") is True,
                "not the normal-build integration target")
        require(Path(target.get("src_path", "")).resolve() == (ROOT / FIXTURE).resolve(),
                "wrong compiled fixture source")
        require(type(entry.get("executable")) is str, "missing executable")
        candidates.append(Path(entry["executable"]))
    require(len(candidates) == 1, "ambiguous compiled artifact")
    return candidates[0]


def binary_digest(path):
    require(path.is_file() and not path.is_symlink() and 0 < path.stat().st_size <= BINARY_LIMIT,
            "invalid or oversized compiled binary")
    total = 0
    result = hashlib.sha256()
    with path.open("rb") as stream:
        while data := stream.read(65536):
            total += len(data)
            require(total <= BINARY_LIMIT, "binary byte limit")
            result.update(data)
    return result.hexdigest()


def log_digest(result):
    # Length framing prevents stdout/stderr concatenation ambiguity.
    left, right = result.stdout.encode(), result.stderr.encode()
    return hashlib.sha256(len(left).to_bytes(8, "big") + left + right).hexdigest()


def validate_report(data):
    report = decode(data, REPORT_LIMIT)
    shape(report, ("schema", "status", "source", "compiled_test", "build", "run", "host",
                   "case", "unexercised", "qualification"))
    exact(report["schema"], "yield.global-recovery-report.v1")
    exact(report["status"], "observed_portable_metadata_case")
    exact(report["qualification"], "observation_only_no_authority_or_matrix_certification")
    exact(report["unexercised"], UNEXERCISED)
    shape(report["case"], CASE)
    for key, value in CASE.items():
        exact(report["case"][key], value)
    source = report["source"]
    shape(source, ("head", "tree", "dirty", "configuration_sha256"))
    digest(source["head"], 40)
    digest(source["tree"], 40)
    exact(source["dirty"], False)
    configs = source["configuration_sha256"]
    shape(configs, ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain",
                   ".cargo/config.toml", ".cargo/config", "crates/yield-library/Cargo.toml",
                   "cargo_home_config", "cargo_home_config_toml"))
    for value in configs.values():
        if value is not None:
            digest(value)
    compiled = report["compiled_test"]
    shape(compiled, ("target", "test", "features", "sha256", "source_head", "source_tree"))
    exact(compiled["target"], TARGET)
    exact(compiled["test"], TEST)
    exact(compiled["features"], [])
    digest(compiled["sha256"])
    exact(compiled["source_head"], source["head"])
    exact(compiled["source_tree"], source["tree"])
    for key, command, ceiling, seconds in (("build", BUILD, BUILD_LIMIT, 600),
                                          ("run", RUN, LOG_LIMIT, 60)):
        stage = report[key]
        shape(stage, ("command", "exit_status", "log_sha256", "log_bytes", "timeout_seconds"))
        exact(stage["command"], command)
        exact(stage["exit_status"], 0)
        exact(stage["timeout_seconds"], seconds)
        require(type(stage["log_bytes"]) is int and 0 <= stage["log_bytes"] <= ceiling,
                "invalid log count")
        digest(stage["log_sha256"])
    host = report["host"]
    shape(host, ("system", "release", "machine", "rustc_version_sha256", "cargo_version_sha256"))
    exact(host["system"], "Linux")
    require(type(host["release"]) is str and re.fullmatch(r"[A-Za-z0-9._+-]{1,128}", host["release"]),
            "invalid host release")
    require(host["machine"] in ("x86_64", "aarch64", "armv7l", "i686", "riscv64", "ppc64le", "s390x"),
            "unsupported host architecture")
    digest(host["rustc_version_sha256"])
    digest(host["cargo_version_sha256"])
    return report


def stage_record(command, result, seconds):
    require(result.returncode == 0, "subprocess failed")
    return {"command": command, "exit_status": result.returncode,
            "log_sha256": log_digest(result),
            "log_bytes": len(result.stdout.encode()) + len(result.stderr.encode()),
            "timeout_seconds": seconds}


def collect():
    require(platform.system() == "Linux", "collector requires Linux; native verification is separate")
    binding = source_binding()
    host = {"system": "Linux", "release": platform.release(), "machine": platform.machine(),
            "rustc_version_sha256": hashlib.sha256(runtime.capture(["rustc", "--version"]).encode()).hexdigest(),
            "cargo_version_sha256": hashlib.sha256(runtime.capture(["cargo", "--version"]).encode()).hexdigest()}
    build = runtime.run_bounded(BUILD, timeout=600, output_limit=BUILD_LIMIT)
    require(build.returncode == 0, "build failed")
    executable = artifact(build.stdout.encode())
    binary = binary_digest(executable)
    require(source_binding() == binding, "source or configuration changed during build")
    run = runtime.run_bounded([str(executable), *RUN[1:]], timeout=60, output_limit=LOG_LIMIT)
    case = parse_case(run.stdout.encode(), run.stderr.encode(), run.returncode)
    require(source_binding() == binding and binary_digest(executable) == binary,
            "source, configuration or binary changed during run")
    report = {"schema": "yield.global-recovery-report.v1", "status": "observed_portable_metadata_case",
              "source": binding, "compiled_test": {"target": TARGET, "test": TEST, "features": [],
                  "sha256": binary, "source_head": binding["head"], "source_tree": binding["tree"]},
              "build": stage_record(BUILD, build, 600), "run": stage_record(RUN, run, 60),
              "host": host, "case": case, "unexercised": UNEXERCISED,
              "qualification": "observation_only_no_authority_or_matrix_certification"}
    encoded = json.dumps(report, sort_keys=True, separators=(",", ":"), allow_nan=False).encode() + b"\n"
    validate_report(encoded)
    return encoded


def publish(output, encoded):
    """Publish a complete validated report exclusively; clean up owned staging."""
    validate_report(encoded)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent, prefix=".global-recovery-",
                                         delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(encoded)
        # Same-directory hard link publishes complete bytes without replacing an
        # existing report; failure leaves no partial public output.
        os.link(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    # Exclusive fresh output outside source avoids a dirty checkout or stale Pass.
    try:
        require(not args.output.resolve().is_relative_to(ROOT.resolve()), "output must be outside checkout")
        require(not args.output.exists(), "output already exists")
        encoded = collect()
        publish(args.output, encoded)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        # Logs/configs/paths are private and never copied into a failed public report.
        parser.exit(2, "collection failed; no success report: " + type(error).__name__ + "\n")
    print("observed_portable_metadata_case; native/physical/matrix qualification remains unexercised")


if __name__ == "__main__":
    main()
