#!/usr/bin/env bash
# Portable development checks; native release acceptance remains on macOS.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

scope="${1:-check}"
case "$scope" in
  check|test) ;;
  *) echo "Usage: scripts/gpui-development-checks.sh [check|test]" >&2; exit 2 ;;
esac

run() {
  echo "+ $*"
  "$@"
}

echo "[gpui-development] host=$(uname -s) head=$(git rev-parse HEAD) scope=$scope"
run cargo fmt --all --check
run cargo check --locked -p yield

if [[ "$scope" == "test" ]]; then
  run cargo test --locked -p yield-core --lib
  run cargo test --locked -p yield-ui --lib filter_family::tests
  run cargo test --locked -p yield-ui --lib enlarged_browser_layout_keeps_toolbar_footer_and_columns_in_viewport
  run cargo test --locked -p yield-library --lib capacity_reserve_namespace
  run cargo test --locked -p yield-library --lib file_ops_journal::recovery_io
  run cargo test --locked -p yield-library --lib recovery_reserve
  # Run process-spawning lock fixtures serially so child startup cannot inherit
  # another fixture's live lock across its clean-handoff/drop assertion.
  run cargo test --locked -p yield-library --lib capacity_volume_owner -- --test-threads=1
  if [[ "$(uname -s)" == "Linux" ]]; then
    run cargo test --locked -p yield-library --lib sqlite_linux
  fi
  run cargo test --locked -p yield-library --test profile_admission
fi

echo "[gpui-development] PASS head=$(git rev-parse HEAD) scope=$scope host=$(uname -s)"
echo "Native macOS CI and hands-on acceptance remain separate: scripts/gpui-local-ci.sh full"
