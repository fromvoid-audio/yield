#!/usr/bin/env bash

# Run the agent request contract:
# 1) run mandatory preflight,
# 2) optionally run the portable regression subset or native macOS full lane.
#
# This script is intentionally small and deterministic so it can be used as the
# first step of each agent request/session.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"

usage() {
  cat <<'USAGE'
Usage: scripts/agent.sh request [--skip-ci] [--quick-ci] [--full-ci]

Run current portable GPUI preflight and explicitly requested regression checks.

Options:
  --skip-ci                 Preflight only (also the default; retained compatibility option).
  --quick-ci                Run the supported scripts/gpui-development-checks.sh test subset.
  --full-ci                 Run scripts/gpui-local-ci.sh full (native macOS only).
  -h, --help                Show this help text.
USAGE
}

SKIP_CI=0
QUICK_CI=0
FULL_CI=0

while (( $# > 0 )); do
  case "$1" in
    --skip-ci)
      SKIP_CI=1
      shift
      ;;
    --quick-ci)
      QUICK_CI=1
      shift
      ;;
    --full-ci)
      FULL_CI=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[agent_request] Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if (( QUICK_CI == 1 && FULL_CI == 1 )); then
  echo "[agent_request] --quick-ci and --full-ci are mutually exclusive." >&2
  usage >&2
  exit 2
fi

./scripts/internal/agent/run_agent_preflight.sh

if (( SKIP_CI == 0 )); then
  if (( FULL_CI == 1 )); then
    ./scripts/gpui-local-ci.sh full
  elif (( QUICK_CI == 1 )); then
    ./scripts/gpui-development-checks.sh test
  fi
fi
