#!/usr/bin/env bash

# Run mandatory agent-facing preflight checks.
#
# This script runs the full guardrail set. Concurrent invocations for one Git
# worktree input image coalesce to one owner so request startup cannot duplicate Cargo
# work with another explicit preflight.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"

usage() {
  cat <<'USAGE'
Usage: scripts/internal/agent/run_agent_preflight.sh

Run the mandatory full preflight checks for an agent request. Concurrent
invocations for the same worktree and unchanged inputs coalesce to one owner.

Options:
  -h, --help             Show this help text.
USAGE
}

while (( $# > 0 )); do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[agent_preflight] Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

STATE_DIR="${YIELD_AGENT_PREFLIGHT_STATE_DIR:-}"
if [[ -z "$STATE_DIR" ]]; then
  GIT_COMMON_DIR="$(git rev-parse --git-common-dir)"
  if [[ "$GIT_COMMON_DIR" == /* ]]; then
    STATE_DIR="$GIT_COMMON_DIR/agent-preflight-state"
  else
    STATE_DIR="$ROOT_DIR/$GIT_COMMON_DIR/agent-preflight-state"
  fi
fi
CHECKS_COMMAND="${YIELD_AGENT_CI_CHECKS_COMMAND:-$ROOT_DIR/scripts/internal/agent/run_agent_ci_checks.sh}"
if [[ ! -x "$CHECKS_COMMAND" ]]; then
  echo "[agent_preflight] ERROR: missing executable full-check command: $CHECKS_COMMAND" >&2
  exit 1
fi

FINGERPRINT_COMMAND="$ROOT_DIR/scripts/internal/agent/agent_preflight_fingerprint.py"
INPUT_FINGERPRINT="$(python3 "$FINGERPRINT_COMMAND" "$ROOT_DIR" "$CHECKS_COMMAND")"
# Even an explicitly shared state directory must not reuse another worktree/head/dirty image.
STATE_DIR="$STATE_DIR/$INPUT_FINGERPRINT"
mkdir -p "$STATE_DIR"

LOCK_FILE="$STATE_DIR/run.lock"
OWNER=0
RESULT_FILE=""

release_lock() {
  local status="$1"
  if (( OWNER == 1 )); then
    printf '%s\n' "$status" > "$RESULT_FILE"
    rm -f "$LOCK_FILE"
    OWNER=0
  fi
}

on_exit() {
  local status=$?
  release_lock "$status"
  exit "$status"
}

trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while :; do
  RESULT_FILE="$STATE_DIR/result.$$.${RANDOM}"
  CANDIDATE_LOCK="$STATE_DIR/candidate.$$.${RANDOM}"
  printf '%s\t%s\n' "$$" "$RESULT_FILE" > "$CANDIDATE_LOCK"
  if ln "$CANDIDATE_LOCK" "$LOCK_FILE" 2>/dev/null; then
    rm -f "$CANDIDATE_LOCK"
    OWNER=1
    if [[ "${YIELD_AGENT_PREFLIGHT_TEST_PAUSE_AFTER_LOCK_ACQUIRE_SECONDS:-0}" != "0" ]]; then
      sleep "$YIELD_AGENT_PREFLIGHT_TEST_PAUSE_AFTER_LOCK_ACQUIRE_SECONDS"
    fi
    break
  fi
  rm -f "$CANDIDATE_LOCK"

  OWNER_PID=""
  OWNER_RESULT=""
  if [[ -r "$LOCK_FILE" ]]; then
    IFS=$'\t' read -r OWNER_PID OWNER_RESULT < "$LOCK_FILE" || true
  fi

  if [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null; then
    echo "[agent_preflight] another full preflight is active (pid $OWNER_PID); waiting to coalesce."
    while [[ -e "$LOCK_FILE" ]] && kill -0 "$OWNER_PID" 2>/dev/null; do
      sleep 0.1
    done
    if [[ -n "$OWNER_RESULT" && -f "$OWNER_RESULT" ]]; then
      OWNER_STATUS="$(<"$OWNER_RESULT")"
      if [[ "$OWNER_STATUS" =~ ^[0-9]+$ ]]; then
        if [[ "$(python3 "$FINGERPRINT_COMMAND" "$ROOT_DIR" "$CHECKS_COMMAND")" != "$INPUT_FINGERPRINT" ]]; then
          echo "[agent_preflight] ERROR: worktree inputs changed while waiting; rerun preflight." >&2
          exit 1
        fi
        echo "[agent_preflight] coalesced with active full preflight (exit $OWNER_STATUS)."
        trap - EXIT
        exit "$OWNER_STATUS"
      fi
    fi
    echo "[agent_preflight] active owner ended without a result; retrying ownership."
    continue
  fi

  echo "[agent_preflight] clearing stale single-flight state."
  rm -f "$LOCK_FILE"
done

echo "[agent_preflight] full preflight owner: $$"
"$CHECKS_COMMAND"

if [[ "$(python3 "$FINGERPRINT_COMMAND" "$ROOT_DIR" "$CHECKS_COMMAND")" != "$INPUT_FINGERPRINT" ]]; then
  echo "[agent_preflight] ERROR: worktree inputs changed during checks; rerun preflight." >&2
  exit 1
fi

echo "[agent_preflight] OK"
