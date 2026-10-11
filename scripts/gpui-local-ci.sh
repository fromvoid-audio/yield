#!/usr/bin/env bash
# GPUI-specific local validation. Run from any directory in this checkout.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "GPUI local CI currently supports macOS only; Windows/Linux validation is deferred until OPT-1863." >&2
  exit 1
fi

repo_root="${YIELD_GPUI_CI_CHECKOUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$repo_root"
common_dir="$(git rev-parse --path-format=absolute --git-common-dir)"
state_dir="$common_dir/local-ci"
source scripts/internal/ci/python_runtime.sh
export YIELD_CI_PYTHON="$(yield_ci_python)"
yield_canonicalize_tmpdir
if [[ -z "${YIELD_LOCAL_CI_COORDINATOR_FD:-}" ]]; then
  exec "$YIELD_CI_PYTHON" scripts/internal/ci/managed_exec.py --timeout-seconds 21600 \
    "$YIELD_CI_PYTHON" scripts/internal/ci/coordinator_exec.py \
    "$state_dir/active.lock" env YIELD_GPUI_CI_COORDINATED=1 bash "$0" "$@"
fi
yield_prepare_python_runtime
if [[ -z "${YIELD_VALIDATION_TARGET_ROOT:-}" ]]; then
  common_dir="$(git rev-parse --path-format=absolute --git-common-dir)"
  repository_root="$(dirname "$common_dir")"
  export YIELD_VALIDATION_TARGET_ROOT="$(dirname "$repository_root")/yield-native-target/validation"
fi

scope="${1:-full}"
case "$scope" in
  full|storage|session) ;;
  *) echo "Usage: scripts/gpui-local-ci.sh [full|storage|session]" >&2; exit 2 ;;
esac

if [[ "$scope" == "full" ]]; then
  metadata_fixture_record="$(
    "$YIELD_CI_PYTHON" scripts/internal/ci/prepare_native_metadata_fixture.py \
      --checkout "$repo_root" --state-dir "$state_dir"
  )"
  export YIELD_NATIVE_METADATA_FIXTURE_ROOT="$(
    "$YIELD_CI_PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["source_root"])' <<<"$metadata_fixture_record"
  )"
  echo "[gpui-local-ci] verified persistent metadata fixture: $metadata_fixture_record"
fi

if [[ "$scope" == "full" && "${YIELD_GPUI_CI_ORCHESTRATED:-0}" != "1" ]]; then
  "$YIELD_CI_PYTHON" scripts/internal/ci/run_full_ci.py
  exit $?
fi

run() {
  echo "+ $*"
  "$@"
}

run_counted() {
  local expected_passed="$1" expected_ignored="$2" output_file status
  shift 2
  output_file="$(mktemp "${TMPDIR:-/tmp}/yield-test-count.XXXXXX")"
  if "$@" >"$output_file" 2>&1; then
    status=0
  else
    status=$?
  fi
  cat "$output_file"
  if (( status == 0 )); then
    if python3 - "$output_file" "$expected_passed" "$expected_ignored" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
results = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", text)
expected = (sys.argv[2], "0", sys.argv[3])
if not results or tuple(results[-1]) != expected:
    raise SystemExit(f"expected exact test counts {expected}, got {results}")
PY
    then
      :
    else
      status=$?
    fi
  fi
  rm -f "$output_file"
  return "$status"
}

echo "[gpui-local-ci] host=$(uname -s) head=$(git rev-parse HEAD) scope=$scope"
run cargo fmt --all --check
if [[ "$scope" == "session" ]]; then
  # Integration-style session tests each start several SQLite-backed owners. Serializing
  # these tests keeps catalog event deadlines reliable under other macOS I/O load.
  run cargo test --locked -p yield-session --lib -- --test-threads=1
  echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
  exit 0
fi
if [[ "$scope" == "full" ]]; then
  run cargo check --locked -p yield
  # The clean-state/admission boundary is part of the local full lane, not a
  # one-off acceptance command. These selectors use disposable roots, then the
  # outer orchestrator retains shared production custody for the suite.
  run_counted 9 1 cargo test --locked -p yield-library --lib app_dirs::maintenance_admission::unix::tests --no-default-features -- --test-threads=1
  run_counted 8 0 cargo test --locked -p yield-analysis --lib app_dirs --features development-state -- --test-threads=1
  if [[ -f vendor/gpui/Cargo.toml ]]; then
    # Exercise authored Metal guards; dependency unit tests are otherwise skipped.
    run cargo test --manifest-path vendor/gpui/Cargo.toml --locked --no-default-features --features font-kit,runtime_shaders --lib post_ -- --test-threads=1
    # Focused native dispatch regressions; physical window/display acceptance is separate.
    run_counted 4 0 cargo test --manifest-path vendor/gpui/Cargo.toml --locked --no-default-features --features font-kit,runtime_shaders --lib native_window_ -- --test-threads=1
    # This harness-free executable runs AppKit window construction on the process main thread.
    run cargo test --manifest-path vendor/gpui/Cargo.toml --locked --no-default-features --features font-kit,runtime_shaders,native-window-selector-test --test native_window_selector_probe
  fi
else
  run cargo check --locked -p yield-library --no-default-features
fi

run cargo test --locked -p yield-library --test profile_admission --no-default-features
run cargo test --locked -p yield-library --lib app_dirs::profile_ownership --no-default-features
run cargo test --locked -p yield-library --test readiness_lease_process --features test-support
# These real SQLite/custody fixtures retain their original short deadlines.
# Bound simultaneous owners instead of letting host CPU count exhaust them.
run cargo test --locked -p yield-library --lib operation_journal --no-default-features -- --test-threads=4 \
  --skip capacity_startup_native_production_rebuilds_original_descriptor_bound_image \
  --skip control_backing_native_production_startup_preserves_original_failure_and_journal \
  --skip namespace_backing_native_production_error_outlives_original_worker_join \
  --skip recording_custody_native_production_original_slot_currentness_and_drain \
  --skip recording_custody_native_production_lost_reply_keeps_original_slot_and_credit \
  --skip chunk_issuance_native_production_original_claim_transition_and_retained_backing \
  --skip provider_reconciliation_native_production_original_queue_refuses_exact_graph_pressure
if [[ "$scope" == "full" && "${YIELD_GPUI_SKIP_NATIVE_CASES:-0}" != "1" ]]; then
  for case in capacity control namespace namespace-poll recording-slot-currentness recording-lost-reply chunk-issuance provider-reconciliation; do
    run "$YIELD_CI_PYTHON" scripts/internal/ci/run_native_production_case.py "$case"
  done
fi
run cargo test --locked -p yield-library --lib file_ops_journal::reader --no-default-features
run cargo test --locked -p yield-library --lib profile_mutation_guard --no-default-features
run cargo test --locked -p yield-library --lib readiness --no-default-features
run cargo test --locked -p yield-library --lib source_database_identity --no-default-features
run cargo test --locked -p yield-library --lib sample_sources::db::tags --no-default-features
run cargo test --locked -p yield-library --lib sample_sources::db::source_db_migration_tests --no-default-features
run cargo test --locked -p yield-library --lib sqlite_wal --no-default-features
run cargo test --locked -p yield-library --lib app_dirs::storage_usage --no-default-features
run cargo test --locked -p yield-scan --lib -- --test-threads=4
# Measure the native query budget without competing catalog fixture workloads.
# Keep the original rows/deadline and require the same test in a separate lane.
harvest_budget_test=tests::harvest_facts_tests::fixtures::harvest_filter_actual_multiple_chunks_and_global_invalidation
run cargo test --locked -p yield-catalog --lib -- --test-threads=4 --skip "$harvest_budget_test"
run_counted 1 0 cargo test --locked -p yield-catalog --lib "$harvest_budget_test" -- --exact --test-threads=1
run cargo test --locked -p yield-session --lib -- --test-threads=1

if [[ "$scope" == "storage" ]]; then
  run cargo test --locked -p yield-core --lib waveform_edit_effects
  run cargo test --locked -p yield-media --lib edit_intent
else
  run cargo test --locked -p reson -p yield-core -p yield-media -p yield-platform --lib
  run cargo test --locked -p yield -p yield-ui --no-run
  run cargo test --locked -p yield-ui --lib
fi

echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
