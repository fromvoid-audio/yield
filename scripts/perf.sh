#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/perf.sh <guard|browser-vm|calibrate-startup|wheel-stability> [args...]

Commands:
  guard              Run the maintained local/manual perf guard.
  browser-vm         Capture 20k BrowserViewModel CPU measurements.
  wheel-stability    Collect wheel-latency stability evidence.
  calibrate-startup  Linux developer-only startup threshold refresh helper.

`calibrate-startup` requires WAYLAND_DISPLAY or DISPLAY and does not represent
shipped Linux product support. Use `scripts/perf.* guard` for release-risk
startup perf evidence on supported app platforms.
EOF
}

if (( $# == 0 )); then
  usage
  exit 0
fi

command="$1"
shift

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/internal/perf"

case "$command" in
  guard) exec "$script_dir/run_perf_guard.sh" "$@" ;;
  browser-vm) exec python3 "$script_dir/gpui_browser_view_model.py" "$@" ;;
  calibrate-startup) exec "$script_dir/calibrate_startup_thresholds.sh" "$@" ;;
  wheel-stability) exec "$script_dir/run_perf_wheel_stability.sh" "$@" ;;
  -h|--help) usage ;;
  *)
    echo "Unknown perf command: $command" >&2
    usage >&2
    exit 2
    ;;
esac
