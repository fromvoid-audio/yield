#!/usr/bin/env bash
# GPUI-specific local validation. Run from any directory in this checkout.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "GPUI local CI currently supports macOS only; Windows/Linux validation is deferred until OPT-1863." >&2
  exit 1
fi

repo_root="${YIELD_GPUI_CI_CHECKOUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
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
  # Each integration-style session test starts several SQLite-backed owners. Keep their
  # startup I/O bounded so the existing five-second event deadlines remain meaningful.
  run cargo test --locked -p yield-session --lib -- --test-threads=4
  echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
  exit 0
fi
if [[ "$scope" == "full" ]]; then
  run cargo check --locked -p yield
else
  run cargo check --locked -p yield-library --no-default-features
fi

run cargo test --locked -p yield-library --test profile_admission --no-default-features
run cargo test --locked -p yield-library --lib app_dirs::profile_ownership --no-default-features
run cargo test --locked -p yield-library --test readiness_lease_process --features test-support
run cargo test --locked -p yield-library --lib operation_journal --no-default-features
run cargo test --locked -p yield-library --lib file_ops_journal::reader --no-default-features
run cargo test --locked -p yield-library --lib profile_mutation_guard --no-default-features
run cargo test --locked -p yield-library --lib readiness --no-default-features
run cargo test --locked -p yield-library --lib source_database_identity --no-default-features
run cargo test --locked -p yield-library --lib sqlite_wal --no-default-features
run cargo test --locked -p yield-library --lib app_dirs::storage_usage --no-default-features
run cargo test --locked -p yield-scan --lib -- --test-threads=4
run cargo test --locked -p yield-catalog --lib
run cargo test --locked -p yield-session --lib -- --test-threads=4

if [[ "$scope" == "storage" ]]; then
  run cargo test --locked -p yield-core --lib waveform_edit_effects
  run cargo test --locked -p yield-media --lib edit_intent
else
  run cargo test --locked -p reson -p yield-core -p yield-media -p yield-platform --lib
  run cargo test --locked -p yield -p yield-ui --no-run
  run cargo test --locked -p yield-ui --lib
fi

echo "[gpui-local-ci] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
