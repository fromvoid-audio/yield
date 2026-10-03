#!/usr/bin/env python3
"""Collect bounded actual catalog analysis observations, never an SLO or UI acknowledgement."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "internal" / "bench"))
import run_source_owner_workload as runtime
import global_recovery_evidence as common

ROOT = Path(__file__).resolve().parents[2]
TARGET = "feature_analysis_evidence"
TEST = "actual_catalog_analysis_outcomes_and_metrics"
FIXTURE = "crates/yield-catalog/tests/" + TARGET + ".rs"
PREFIX = "YIELD_CATALOG_ANALYSIS_EVIDENCE="
BUILD = ["cargo", "test", "--locked", "-p", "yield-catalog", "--no-default-features",
         "--test", TARGET, "--no-run", "--message-format=json"]
RUN = ["<compiled-test>", "--exact", TEST, "--ignored", "--nocapture", "--test-threads=1"]
BUILD_LIMIT = 4 * 1024 * 1024
LOG_LIMIT = 256 * 1024
CASE_LIMIT = 16 * 1024
REPORT_LIMIT = 32 * 1024
TOTAL_SECONDS = 900
COUNT_FIELDS = ("entered", "dropped_before_work", "terminal_ready", "terminal_blocked",
                "terminal_stale", "terminal_cancelled", "terminal_enqueued", "terminal_projection_rejected")
METRIC_FIELDS = (*COUNT_FIELDS, "total_elapsed_us", "max_elapsed_us", "saturated")
SQL_FIELDS = ("payload_rows", "payload_bytes", "marker_rows", "failed_jobs", "retryable_jobs")
ZERO_SQL = dict.fromkeys(SQL_FIELDS, 0)
READY_SQL = {**ZERO_SQL, "payload_rows": 1, "payload_bytes": 732, "marker_rows": 1}
FAILED_SQL = {**ZERO_SQL, "failed_jobs": 1, "retryable_jobs": 1}
CASES = (("ready", "ready", ZERO_SQL, READY_SQL, 6),
         ("already_ready", "ready", READY_SQL, READY_SQL, 3),
         ("invalid_wav", "blocked_computation", ZERO_SQL, FAILED_SQL, 6),
         ("stale_revision", "blocked_stale", READY_SQL, READY_SQL, 1),
         ("cancelled", "blocked_cancelled", READY_SQL, READY_SQL, 1))
LIMITS = ["producer_enqueue_is_not_ui_acknowledgement", "timings_are_not_hardware_independent_slo",
          "normal_build_catalog_without_journal_client", "existing_library_test_support_dev_dependency",
          "no_native_gpui_or_held_input_qualification", "no_physical_capacity_or_power_loss_claim",
          "no_full_replay_or_failure_matrix_certification"]


def require(condition, code):
    if not condition:
        raise ValueError(code)


def integer(value, maximum=(1 << 64) - 2):
    require(type(value) is int and 0 <= value <= maximum, "invalid_or_saturated_integer")
    return value


def validate_metrics(value):
    common.shape(value, METRIC_FIELDS)
    for field in METRIC_FIELDS[:-1]:
        integer(value[field])
    common.exact(value["saturated"], False)
    require(value["entered"] == value["dropped_before_work"] + value["terminal_ready"] + value["terminal_blocked"], "unsettled_metrics")
    require(value["terminal_enqueued"] + value["terminal_projection_rejected"] == value["terminal_ready"] + value["terminal_blocked"], "projection_partition")
    require(value["terminal_stale"] + value["terminal_cancelled"] <= value["terminal_blocked"], "blocked_subset")
    require(value["max_elapsed_us"] <= value["total_elapsed_us"], "elapsed_maximum")
    return value


def validate_sql(value, expected):
    common.shape(value, SQL_FIELDS)
    for field in SQL_FIELDS:
        integer(value[field], 8192)
        common.exact(value[field], expected[field])


def validate_case(record):
    common.shape(record, ("schema", "fixture", "files", "requests", "frame_count", "sample_rate", "samples",
                          "ready_key_reused", "reset", "restart_sql_retained", "ui_acknowledged", "physical_qualification", "slo_claim"))
    for key, value in {"schema": "yield.catalog-analysis-case.v1", "fixture": "actual-normal-build-catalog-v1",
                       "files": 2, "requests": 5, "frame_count": 16000, "sample_rate": 16000,
                       "ready_key_reused": True, "restart_sql_retained": True,
                       "ui_acknowledged": False, "physical_qualification": False, "slo_claim": False}.items():
        common.exact(record[key], value)
    common.exact(validate_metrics(record["reset"]), {**dict.fromkeys(METRIC_FIELDS[:-1], 0), "saturated": False})
    samples = record["samples"]
    require(type(samples) is list and len(samples) == 5, "missing_or_duplicate_samples")
    previous = {**dict.fromkeys(METRIC_FIELDS[:-1], 0), "saturated": False}
    for sample, (name, result, before_sql, after_sql, events) in zip(samples, CASES):
        common.shape(sample, ("case", "result", "before", "after", "sql_before", "sql_after", "received_events", "wall_us"))
        common.exact(sample["case"], name)
        common.exact(sample["result"], result)
        common.exact(sample["received_events"], events)
        wall = integer(sample["wall_us"], 5_000_000)
        before, after = validate_metrics(sample["before"]), validate_metrics(sample["after"])
        common.exact(before, previous)
        expected = {field: 0 for field in COUNT_FIELDS}
        expected.update(entered=1, terminal_enqueued=1)
        expected["terminal_ready" if result == "ready" else "terminal_blocked"] = 1
        if result == "blocked_stale": expected["terminal_stale"] = 1
        if result == "blocked_cancelled": expected["terminal_cancelled"] = 1
        for field in COUNT_FIELDS:
            common.exact(after[field] - before[field], expected[field])
        elapsed = integer(after["total_elapsed_us"] - before["total_elapsed_us"], wall)
        require(after["max_elapsed_us"] == max(before["max_elapsed_us"], elapsed), "inconsistent_elapsed_delta")
        validate_sql(sample["sql_before"], before_sql)
        validate_sql(sample["sql_after"], after_sql)
        previous = after
    return record


def parse_case(stdout, stderr, status):
    require(type(status) is int and status == 0, "fixture_process_failed")
    require(len(stdout) + len(stderr) <= LOG_LIMIT, "fixture_log_limit")
    require(stdout.count(PREFIX.encode()) == 1 and PREFIX.encode() not in stderr, "ambiguous_case_marker")
    records = [line[len(PREFIX):] for line in stdout.decode("utf-8").splitlines() if line.startswith(PREFIX)]
    require(len(records) == 1, "case_marker_boundary")
    return validate_case(common.decode(records[0].encode(), CASE_LIMIT))


def binding():
    require(not runtime.capture(["git", "status", "--porcelain", "--untracked-files=all"]), "dirty_checkout")
    configuration = runtime.build_configuration()
    path = ROOT / "crates/yield-catalog/Cargo.toml"
    require(path.stat().st_size <= 1024 * 1024, "catalog_config_limit")
    configuration["crates/yield-catalog/Cargo.toml"] = hashlib.sha256(path.read_bytes()).hexdigest()
    return {"head": runtime.capture(["git", "rev-parse", "HEAD"]), "tree": runtime.capture(["git", "rev-parse", "HEAD^{tree}"]),
            "dirty": False, "configuration_sha256": configuration}


def artifact(stdout):
    require(len(stdout) <= BUILD_LIMIT, "build_log_limit")
    candidates = []
    normal_catalog = False
    for line in stdout.splitlines():
        item = common.decode(line, BUILD_LIMIT)
        if item.get("reason") != "compiler-artifact": continue
        target = item.get("target", {})
        if target.get("name") == "yield_catalog" and target.get("kind") == ["lib"]:
            require(item.get("features") == [] and item.get("profile", {}).get("test") is False, "catalog_is_not_normal_build")
            normal_catalog = True
        if target.get("name") != TARGET: continue
        require(target.get("kind") == ["test"] and item.get("features") == [] and item.get("profile", {}).get("test") is True, "incorrect_fixture_build")
        require(Path(target.get("src_path", "")).resolve() == (ROOT / FIXTURE).resolve(), "incorrect_fixture_source")
        require(type(item.get("executable")) is str, "missing_executable")
        candidates.append(Path(item["executable"]))
    require(normal_catalog and len(candidates) == 1, "ambiguous_or_unproven_normal_build")
    return candidates[0]


def validate_report(data):
    report = common.decode(data, REPORT_LIMIT)
    common.shape(report, ("schema", "status", "source", "compiled_test", "build", "run", "host", "case", "limits", "qualification"))
    common.exact(report["schema"], "yield.catalog-analysis-report.v1")
    common.exact(report["status"], "observed_actual_analysis_cases")
    common.exact(report["qualification"], "observations_only_no_slo_or_authority")
    common.exact(report["limits"], LIMITS)
    validate_case(report["case"])
    source = report["source"]
    common.shape(source, ("head", "tree", "dirty", "configuration_sha256"))
    common.digest(source["head"], 40); common.digest(source["tree"], 40); common.exact(source["dirty"], False)
    names = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", ".cargo/config.toml", ".cargo/config",
             "crates/yield-library/Cargo.toml", "cargo_home_config", "cargo_home_config_toml", "crates/yield-catalog/Cargo.toml")
    common.shape(source["configuration_sha256"], names)
    for value in source["configuration_sha256"].values():
        if value is not None: common.digest(value)
    for name in ("Cargo.toml", "Cargo.lock", "crates/yield-library/Cargo.toml", "crates/yield-catalog/Cargo.toml"):
        common.digest(source["configuration_sha256"][name])
    compiled = report["compiled_test"]
    common.shape(compiled, ("target", "test", "features", "catalog_cfg_test", "sha256", "source_head", "source_tree"))
    common.exact(compiled["target"], TARGET); common.exact(compiled["test"], TEST)
    common.exact(compiled["features"], []); common.exact(compiled["catalog_cfg_test"], False)
    common.digest(compiled["sha256"])
    common.exact(compiled["source_head"], source["head"]); common.exact(compiled["source_tree"], source["tree"])
    for key, command, ceiling, seconds in (("build", BUILD, BUILD_LIMIT, 600), ("run", RUN, LOG_LIMIT, 60)):
        stage = report[key]
        common.shape(stage, ("command", "exit_status", "log_sha256", "log_bytes", "timeout_seconds"))
        common.exact(stage["command"], command); common.exact(stage["exit_status"], 0)
        common.exact(stage["timeout_seconds"], seconds); integer(stage["log_bytes"], ceiling); common.digest(stage["log_sha256"])
    host = report["host"]
    common.shape(host, ("system", "release", "machine", "rustc_version", "cargo_version", "cargo_incremental"))
    common.exact(host["system"], "Linux")
    for field, maximum in (("release", 128), ("machine", 32), ("rustc_version", 180), ("cargo_version", 180)):
        value = host[field]
        require(type(value) is str and re.fullmatch(r"[A-Za-z0-9 ._()+-]{1,%d}" % maximum, value), "invalid_host_context")
    for field, name in (("rustc_version", "rustc"), ("cargo_version", "cargo")):
        require(re.fullmatch(name + r" \d+\.\d+\.\d+(?:[-A-Za-z0-9.]+)? \([0-9a-f]{7,40} \d{4}-\d{2}-\d{2}\)", host[field]), "missing_toolchain")
    require(host["cargo_incremental"] in (None, "0", "1"), "invalid_incremental_context")
    return report


def collect():
    # POSIX alarm bounds the whole collection, including metadata commands and hashing.
    def timeout(_signal, _frame): raise RuntimeError("collector_total_timeout")
    previous = signal.signal(signal.SIGALRM, timeout)
    signal.setitimer(signal.ITIMER_REAL, TOTAL_SECONDS)
    try:
        return collect_bounded()
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


def collect_bounded():
    start = time.monotonic()
    def budget(): require(time.monotonic() - start <= TOTAL_SECONDS, "collector_total_timeout")
    require(platform.system() == "Linux", "collector_requires_linux")
    source = binding(); budget()
    host = {"system":"Linux", "release":platform.release(), "machine":platform.machine(),
            "rustc_version":runtime.capture(["rustc","--version"]), "cargo_version":runtime.capture(["cargo","--version"]),
            "cargo_incremental":os.environ.get("CARGO_INCREMENTAL")}
    build = runtime.run_bounded(BUILD, timeout=600, output_limit=BUILD_LIMIT); budget()
    require(build.returncode == 0, "fixture_build_failed")
    executable = artifact(build.stdout.encode())
    binary = common.binary_digest(executable)
    require(binding() == source, "source_changed_during_build"); budget()
    run = runtime.run_bounded([str(executable),*RUN[1:]], timeout=60, output_limit=LOG_LIMIT); budget()
    case = parse_case(run.stdout.encode(),run.stderr.encode(),run.returncode)
    require(binding() == source and common.binary_digest(executable) == binary, "source_or_binary_changed_during_run"); budget()
    report = {"schema":"yield.catalog-analysis-report.v1","status":"observed_actual_analysis_cases", "source":source,
        "compiled_test":{"target":TARGET,"test":TEST,"features":[],"catalog_cfg_test":False,"sha256":binary,"source_head":source["head"],"source_tree":source["tree"]},
        "build":common.stage_record(BUILD,build,600),"run":common.stage_record(RUN,run,60),"host":host,"case":case,"limits":LIMITS,
        "qualification":"observations_only_no_slo_or_authority"}
    encoded = json.dumps(report,sort_keys=True,separators=(",",":"),allow_nan=False).encode()+b"\n"
    validate_report(encoded)
    return encoded


def publish(output, encoded):
    validate_report(encoded)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent,prefix=".catalog-analysis-",delete=False) as stream:
            temporary = Path(stream.name); stream.write(encoded)
        os.link(temporary,output)
    finally:
        if temporary is not None: temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output",type=Path,required=True)
    args = parser.parse_args()
    try:
        require(not args.output.resolve().is_relative_to(ROOT.resolve()),"output_inside_checkout")
        require(not args.output.exists(),"output_exists")
        publish(args.output,collect())
    except (OSError,ValueError,RuntimeError,subprocess.SubprocessError) as error:
        # Fixed diagnostic class/code only: paths, config contents and raw logs stay private.
        code = str(error) if isinstance(error,ValueError) and re.fullmatch(r"[a-z_]{1,80}",str(error)) else type(error).__name__
        parser.exit(2,"collection failed: "+code+"; no success report\n")
    print("observed_actual_analysis_cases; no SLO, UI acknowledgement or physical qualification")


if __name__ == "__main__": main()
