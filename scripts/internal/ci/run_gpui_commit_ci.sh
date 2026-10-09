#!/usr/bin/env bash
# Validate the exact committed tree from a post-commit hook, outside dirty worktrees.
set -euo pipefail

locked=0
if [[ "${1:-}" == "--locked" ]]; then
  locked=1
  shift
fi
if (( $# != 2 )); then
  echo "Usage: run_gpui_commit_ci.sh [--locked] <checkout> <commit>" >&2
  exit 2
fi

source_checkout="$1"
commit="$2"
# Git exports repository-local variables while invoking hooks. They otherwise
# override `git -C` and break detached worktree creation.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_PREFIX
source "$source_checkout/scripts/internal/ci/python_runtime.sh"
export YIELD_CI_PYTHON="$(yield_ci_python)"
case "$commit" in
  *[!0-9a-f]*|'') echo "Expected a full hexadecimal commit ID" >&2; exit 2 ;;
esac
if (( ${#commit} != 40 )); then
  echo "Expected a full 40-character commit ID" >&2
  exit 2
fi

common_dir="$(git -C "$source_checkout" rev-parse --path-format=absolute --git-common-dir)"
state_dir="$common_dir/local-ci"
run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$"
run_dir="$state_dir/runs/$commit/$run_id"
mkdir -p "$run_dir" "$state_dir/target"
host="$(uname -s)"

# One writer owns the shared Cargo target at a time. All local CI entrypoints use
# the same Python fcntl.flock lock descriptor; lockf record locks are a different
# primitive and would not serialize with namespace maintenance or the local tool.
if (( locked == 0 )); then
  exec "$YIELD_CI_PYTHON" "$source_checkout/scripts/internal/ci/coordinator_exec.py" \
    "$state_dir/active.lock" bash "$0" --locked "$source_checkout" "$commit"
fi
yield_prepare_python_runtime
tmp_parent=""
cleanup() {
  result=$?
  if (( result != 0 )) && { [[ ! -f "$run_dir/result" ]] || grep -q '^RUNNING ' "$run_dir/result"; }; then
    printf 'FAIL %s exit=%d\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$result" > "$run_dir/result"
  fi
  if [[ -n "$tmp_parent" ]]; then
    git -C "$source_checkout" worktree remove --force "$tmp_parent/checkout" >/dev/null 2>&1 || true
    rmdir "$tmp_parent" 2>/dev/null || true
  fi
  if [[ -n "${YIELD_CI_PYTHON_RUNTIME_DIR:-}" ]]; then
    rm -f "$YIELD_CI_PYTHON_RUNTIME_DIR/python3"
    rmdir "$YIELD_CI_PYTHON_RUNTIME_DIR" 2>/dev/null || true
  fi
}
trap cleanup EXIT

printf 'RUNNING %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$run_dir/result"
tmp_parent="$(mktemp -d "${TMPDIR:-/tmp}/yield-ci.XXXXXX")"
git -C "$source_checkout" worktree add --detach "$tmp_parent/checkout" "$commit" > "$run_dir/worktree.log" 2>&1
ci_script="$tmp_parent/checkout/scripts/gpui-local-ci.sh"
if ! grep -q 'acquire_process_maintenance_admission' "$tmp_parent/checkout/crates/yield/src/main.rs"; then
  echo "refusing historical commit without the process-lifetime maintenance admission protocol" >&2
  exit 1
fi
if [[ ! -f "$tmp_parent/checkout/scripts/internal/ci/managed_exec.py" ]]; then
  echo "frozen checkout lacks the process-group CI watchdog" >&2
  exit 1
fi
if [[ ! -f "$tmp_parent/checkout/scripts/internal/ci/run_full_ci.py" || ! -f "$tmp_parent/checkout/scripts/internal/ci/python_runtime.sh" ]]; then
  echo "frozen checkout lacks reversible full-CI orchestration or its Python runtime helper" >&2
  exit 1
fi
ci_script_hash="$(shasum -a 256 "$ci_script" | awk '{print $1}')"

repository_root="$(dirname "$common_dir")"
shared_target_root="$(dirname "$repository_root")/yield-native-target/validation"
lock_hash="$(shasum -a 256 "$tmp_parent/checkout/Cargo.lock" | awk '{print substr($1, 1, 12)}')"
rust_identity="$(rustc -vV | awk '/^(release|host):/{print $2}' | tr '\n' '-' | sed 's/-$//')"
export YIELD_VALIDATION_TARGET_ROOT="$shared_target_root"
export CARGO_TARGET_DIR="$shared_target_root/$rust_identity-$lock_hash"
export YIELD_GPUI_CI_CHECKOUT="$tmp_parent/checkout"
set +e
"$YIELD_CI_PYTHON" "$tmp_parent/checkout/scripts/internal/ci/managed_exec.py" \
  --timeout-seconds 21600 bash "$ci_script" full > "$run_dir/check.log" 2>&1
result=$?
set -e
if (( result == 0 )); then
  printf 'PASS %s host=%s script=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$host" "$ci_script_hash" > "$run_dir/result"
else
  printf 'FAIL %s host=%s script=%s exit=%d\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$host" "$ci_script_hash" "$result" > "$run_dir/result"
fi
exit "$result"
