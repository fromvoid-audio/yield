#!/usr/bin/env bash
# GPUI-specific local validation. Run from any directory in this checkout.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "GPUI local CI currently supports macOS only; Windows/Linux validation is deferred until OPT-1863." >&2
  exit 1
fi

repo_root="${WAVECRATE_GPUI_CI_CHECKOUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$repo_root"

scope="${1:-full}"
case "$scope" in
  full|storage|session) ;;
  *) echo "Usage: scripts/gpui-local-ci.sh [full|storage|session]" >&2; exit 2 ;;
esac

run() {
  echo "+ $*"
  "$@"
}

echo "[gpui-local-ci] host=$(uname -s) head=$(git rev-parse HEAD) scope=$scope"
run cargo fmt --all --check
if [[ "$scope" == "session" ]]; then
  run cargo test --locked -p wavecrate-session --lib
  echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
  exit 0
fi
if [[ "$scope" == "full" ]]; then
  run cargo check --locked -p wavecrate-gpui
else
  run cargo check --locked -p wavecrate-library --no-default-features
fi

run cargo test --locked -p wavecrate-library --test profile_admission --no-default-features
run cargo test --locked -p wavecrate-library --lib app_dirs::profile_ownership --no-default-features
run cargo test --locked -p wavecrate-library --test readiness_lease_process --features test-support
run cargo test --locked -p wavecrate-library --lib operation_journal --no-default-features
run cargo test --locked -p wavecrate-library --lib file_ops_journal::reader --no-default-features
run cargo test --locked -p wavecrate-library --lib profile_mutation_guard --no-default-features
run cargo test --locked -p wavecrate-library --lib readiness --no-default-features
run cargo test --locked -p wavecrate-library --lib source_database_identity --no-default-features
run cargo test --locked -p wavecrate-library --lib sqlite_wal --no-default-features
run cargo test --locked -p wavecrate-library --lib app_dirs::storage_usage --no-default-features
run cargo test --locked -p wavecrate-catalog --lib
# The current main-branch session fixture waits for events that do not arrive
# on macOS. Keep the code compiled and track the suite repair in OPT-1861.
run cargo check --locked -p wavecrate-session
echo "[gpui-local-ci] macOS session tests skipped: baseline event timeouts; run scope=session to reproduce"

if [[ "$scope" == "storage" ]]; then
  run cargo test --locked -p wavecrate-core --lib waveform_edit_effects
  run cargo test --locked -p wavecrate-media --lib edit_intent
else
  run cargo test --locked -p reson -p wavecrate-core -p wavecrate-media -p wavecrate-platform --lib
  run cargo test --locked -p wavecrate-gpui -p wavecrate-ui --no-run
  run cargo test --locked -p wavecrate-ui --lib
fi

echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
