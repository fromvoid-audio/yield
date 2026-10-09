#!/usr/bin/env bash
# Install a post-commit hook for this repository's shared worktrees.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "GPUI local CI hook installation currently supports macOS only; Windows/Linux is deferred until OPT-1863." >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
common_dir="$(git -C "$repo_root" rev-parse --path-format=absolute --git-common-dir)"
hook="$common_dir/hooks/post-commit"
marker="# yield-local-ci"

for tool in cargo shasum; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Local CI requires $tool" >&2
    exit 1
  fi
done
source "$repo_root/scripts/internal/ci/python_runtime.sh"
ci_python="$(yield_ci_python)"
if ! "$ci_python" -c 'import tomllib' >/dev/null 2>&1; then
  echo "Selected Python runtime cannot import tomllib: $ci_python" >&2
  exit 1
fi

if [[ -e "$hook" ]] && ! grep -qF "$marker" "$hook" && ! grep -qE '^# [a-z]+-gpui-local-ci$' "$hook"; then
  echo "Existing post-commit hook is not managed by GPUI local CI: $hook" >&2
  exit 1
fi

mkdir -p "$(dirname "$hook")" "$common_dir/local-ci/bin"
cp "$repo_root/scripts/internal/ci/run_gpui_commit_ci.sh" "$common_dir/local-ci/bin/run_gpui_commit_ci.sh"
chmod +x "$common_dir/local-ci/bin/run_gpui_commit_ci.sh"
cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
# yield-local-ci
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
commit="$(git rev-parse HEAD)"
common_dir="$(git rev-parse --path-format=absolute --git-common-dir)"
runner="$common_dir/local-ci/bin/run_gpui_commit_ci.sh"
if [[ ! -f "$runner" ]]; then
  exit 0
fi
echo "[gpui-local-ci] checking $commit synchronously; log: $common_dir/local-ci/runs/$commit/<run-id>/check.log"
if bash "$runner" "$repo_root" "$commit"; then
  echo "[gpui-local-ci] PASS $commit"
else
  echo "[gpui-local-ci] FAIL $commit; see a run record under $common_dir/local-ci/runs/$commit/" >&2
fi
HOOK
chmod +x "$hook"
echo "Installed $hook"
echo "Each commit now runs local GPUI CI in a clean snapshot before returning."
echo "Synchronous post-commit check; results: $common_dir/local-ci/runs/<commit>/<run-id>/{result,check.log}"
