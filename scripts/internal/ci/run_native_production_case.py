#!/usr/bin/env python3
"""Run one exact native-production selector inside a reversible state transaction."""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from runtime_paths import canonicalize_tmpdir  # noqa: E402

canonicalize_tmpdir()
import local_state  # noqa: E402
from process_tree import Cancelled, catch_cancellation_signals, ignore_cancellation_signals  # noqa: E402

CASES = {
    "capacity": "operation_journal::owner::tests::capacity_startup_tests::capacity_startup_native_production_rebuilds_original_descriptor_bound_image",
    "control": "operation_journal::owner::tests::control_backing_tests::control_backing_native_production_startup_preserves_original_failure_and_journal",
    "namespace": "operation_journal::owner::tests::namespace_backing_tests::namespace_backing_native_production_error_outlives_original_worker_join",
    "recording-slot-currentness": "operation_journal::owner::recording_binding::tests::recording_custody_native_production_original_slot_currentness_and_drain",
    "recording-lost-reply": "operation_journal::owner::recording_binding::tests::recording_custody_native_production_lost_reply_keeps_original_slot_and_credit",
    "chunk-issuance": "operation_journal::owner::recording_binding::tests::chunk_issuance_native_production_original_claim_transition_and_retained_backing",
    "provider-reconciliation": "operation_journal::owner::tests::provider_reconciliation_tests::provider_reconciliation_native_production_original_queue_refuses_exact_graph_pressure",
}


def selector_output_is_exactly_one_success(selector: str, output: str) -> bool:
    return bool(
        re.search(r"(?m)^running 1 test$", output)
        and re.search(rf"(?m)^test {re.escape(selector)} \.\.\. ok$", output)
        and re.search(
            r"(?m)^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out; finished in ",
            output,
        )
    )


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] not in CASES:
        print("usage: run_native_production_case.py <" + "|".join(CASES) + ">", file=sys.stderr)
        return 2
    if os.environ.get("YIELD_CONFIG_HOME") or os.environ.get("YIELD_CONFIG_PROFILE"):
        print("clear YIELD_CONFIG_HOME and YIELD_CONFIG_PROFILE before native production validation", file=sys.stderr)
        return 2

    checkout = pathlib.Path(os.environ.get("YIELD_GPUI_CI_CHECKOUT", pathlib.Path(__file__).resolve().parents[3]))
    case_name = sys.argv[1]
    selector = CASES[case_name]
    command = [
        "cargo", "test", "--locked", "-p", "yield-library", "--lib", "--no-default-features",
        selector, "--", "--exact", "--test-threads=1",
    ]
    transaction = None
    namespace_fd = transition_fd = None
    process = None
    status = 1
    failure = None
    env = os.environ.copy()
    env.pop("YIELD_MAINTENANCE_ADMISSION_FD", None)
    env.pop("YIELD_MAINTENANCE_TRANSITION_FD", None)
    try:
        transaction, namespace_fd, transition_fd = local_state.prepare_clean_launch(
            "production", artifact=f"full-native:{case_name}:{selector}"
        )
        env["YIELD_MAINTENANCE_ADMISSION_FD"] = str(namespace_fd)
        env["YIELD_MAINTENANCE_TRANSITION_FD"] = str(transition_fd)
        print(json.dumps({"case": case_name, "selector": selector, "transaction": transaction}, indent=2), flush=True)
        inherited_fds = [namespace_fd, transition_fd]
        coordinator_fd = env.get("YIELD_LOCAL_CI_COORDINATOR_FD")
        if coordinator_fd is not None:
            inherited_fds.append(int(coordinator_fd))
        process = subprocess.Popen(
            command,
            cwd=checkout,
            env=env,
            pass_fds=tuple(inherited_fds),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        try:
            with catch_cancellation_signals():
                output, _ = process.communicate()
        except Cancelled as cancelled:
            ignore_cancellation_signals()
            failure = f"native production selector cancelled by signal {cancelled.signum}"
            status = 128 + cancelled.signum
            output = b""
        decoded = output.decode("utf-8", errors="replace")
        sys.stdout.write(decoded)
        sys.stdout.flush()
        if failure is None:
            status = process.returncode
        if status == 0:
            if not selector_output_is_exactly_one_success(selector, decoded):
                failure = "Cargo exited successfully without evidence that exactly the requested selector ran once"
                status = 1
    except KeyboardInterrupt:
        failure = "interrupted"
        status = 130
    except BaseException as exc:
        failure = f"{type(exc).__name__}: {exc}"
    finally:
        if namespace_fd is not None:
            os.close(namespace_fd)
        if transition_fd is not None:
            os.close(transition_fd)
        if transaction and transaction.get("transaction"):
            try:
                restored = local_state.restore(str(transaction["transaction"]))
                print(json.dumps({"restore": restored}, indent=2), flush=True)
            except BaseException as exc:
                failure = f"state restore failed: {type(exc).__name__}: {exc}"
                status = 1
        record = {
            "schema": 1,
            "case": case_name,
            "selector": selector,
            "command": command,
            "exit_code": status,
            "failure": failure,
            "selector_execution_verified": status == 0,
            "expected_executed_tests": 1,
            "completed_unix_ns": time.time_ns(),
        }
        print(json.dumps(record, indent=2), flush=True)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
