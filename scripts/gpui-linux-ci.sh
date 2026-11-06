#!/usr/bin/env bash
# Linux product validation; no production-state parking or hosted CI.
set -euo pipefail

usage() {
  echo "Usage: scripts/gpui-linux-ci.sh [check|full]"
}

if (( $# > 1 )); then
  usage >&2
  exit 2
fi
scope="${1:-full}"
case "$scope" in
  -h|--help) usage; exit 0 ;;
  check|full) ;;
  *) usage >&2; exit 2 ;;
esac
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "The Linux product lane requires a Linux host." >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
run() {
  echo "+ $*"
  "$@"
}

echo "[gpui-linux-ci] host=$(uname -sm) head=$(git rev-parse HEAD) scope=$scope"
run cargo fmt --all --check
# Check the shipped graph separately: test-support must not be what enables
# the X11/Wayland backends in a normal application build.
run cargo check --locked -p yield --bin yield
if [[ "$scope" == "full" ]]; then
  run cargo check --locked --workspace --all-targets
  # Keep real owner/process fixtures and their original deadlines. Containers
  # without a reaping init need a subreaper for deliberately orphaned children.
  reaper=()
  if command -v tini >/dev/null 2>&1; then reaper=(tini -s --); fi
  run "${reaper[@]}" python3 -m unittest scripts.internal.ci.test_linux_ci_runner \
    scripts.internal.ci.test_full_ci_runner scripts.internal.ci.test_runtime_paths
  run "${reaper[@]}" cargo test --workspace --locked --no-fail-fast -- --test-threads=1
fi
echo "[gpui-linux-ci] PASS head=$(git rev-parse HEAD) scope=$scope"
echo "Desktop, audio-device and other-platform acceptance is recorded separately in Pulse."
