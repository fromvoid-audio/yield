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
case "$commit" in
  *[!0-9a-f]*|'') echo "Expected a full hexadecimal commit ID" >&2; exit 2 ;;
esac
if (( ${#commit} != 40 )); then
  echo "Expected a full 40-character commit ID" >&2
  exit 2
fi

common_dir="$(git -C "$source_checkout" rev-parse --path-format=absolute --git-common-dir)"
state_dir="$common_dir/local-ci"
mkdir -p "$state_dir/runs" "$state_dir/target"
run_dir="$state_dir/runs/$commit"
mkdir -p "$run_dir"
ci_script="${WAVECRATE_GPUI_CI_SCRIPT:-$source_checkout/scripts/gpui-local-ci.sh}"
ci_script_hash="$(shasum -a 256 "$ci_script" | awk '{print $1}')"
host="$(uname -s)"

# One writer owns the shared Cargo target at a time. OS locks recover on crash.
if (( locked == 0 )); then
  if command -v lockf >/dev/null 2>&1; then
    exec lockf -k "$state_dir/active.lock" bash "$0" --locked "$source_checkout" "$commit"
  elif command -v flock >/dev/null 2>&1; then
    exec flock "$state_dir/active.lock" bash "$0" --locked "$source_checkout" "$commit"
  else
    echo "No local file-lock tool available (lockf or flock)" >&2
    exit 1
  fi
fi
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
}
trap cleanup EXIT

if [[ -f "$run_dir/result" ]] && grep -q '^PASS ' "$run_dir/result" && grep -qF "host=$host script=$ci_script_hash" "$run_dir/result"; then
  exit 0
fi

printf 'RUNNING %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$run_dir/result"
tmp_parent="$(mktemp -d "${TMPDIR:-/tmp}/wavecrate-gpui-ci.XXXXXX")"
git -C "$source_checkout" worktree add --detach "$tmp_parent/checkout" "$commit" > "$run_dir/worktree.log" 2>&1

export CARGO_TARGET_DIR="$state_dir/target"
export WAVECRATE_GPUI_CI_CHECKOUT="$tmp_parent/checkout"
set +e
bash "$ci_script" full > "$run_dir/check.log" 2>&1
result=$?
set -e
if (( result == 0 )); then
  printf 'PASS %s host=%s script=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$host" "$ci_script_hash" > "$run_dir/result"
else
  printf 'FAIL %s host=%s script=%s exit=%d\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$host" "$ci_script_hash" "$result" > "$run_dir/result"
fi
exit "$result"
