#!/usr/bin/env bash

# Current portable GPUI workspace preflight for agent-driven work.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"

usage() {
  cat <<'USAGE'
Usage: scripts/internal/agent/run_agent_ci_checks.sh

Run active-workflow pinning and the canonical portable GPUI workspace preflight.

Options:
  --help                 Show this help text.
USAGE
}

while (( $# > 0 )); do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[agent_ci] Unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

# One canonical workspace lane. Archived monolith checks are not current acceptance.
"$ROOT_DIR/scripts/internal/check/check_workflow_toolchain_pinning.sh"
exec "$ROOT_DIR/scripts/gpui-development-checks.sh" preflight
