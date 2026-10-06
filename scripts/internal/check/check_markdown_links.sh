#!/usr/bin/env bash

# Diff-aware local Markdown link checker.
#
# Checks only added/modified Markdown files (plus staged/unstaged edits) for
# broken local file links so new link rot doesn't get introduced.
#
# What is checked:
# - Relative links to local files (e.g. `docs/FOO.md`, `../manual/usage.md`)
#
# What is ignored:
# - HTTP(S), mailto, tel
# - Absolute site links starting with `/`
# - Pure anchors starting with `#`

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"
if [[ -f "$ROOT_DIR/scripts/internal/git_diff_env.sh" ]]; then
  # shellcheck source=scripts/internal/git_diff_env.sh
  source "$ROOT_DIR/scripts/internal/git_diff_env.sh"
else
  yield_git() {
    git "$@"
  }
fi

BASE_REF=""
HEAD_REF="HEAD"
HEAD_EXPLICIT=0

usage() {
  cat <<'EOF'
Usage: scripts/internal/check/check_markdown_links.sh [--base <ref>] [--head <ref>]

Checks changed Markdown files for broken local file links.
EOF
}

while (( $# > 0 )); do
  case "$1" in
    --base)
      if (( $# < 2 )) || [[ -z "$2" || "$2" == --* ]]; then
        echo "[md_links] --base requires a commit reference" >&2; exit 2
      fi
      BASE_REF="$2"; shift 2 ;;
    --head)
      if (( $# < 2 )) || [[ -z "$2" || "$2" == --* ]]; then
        echo "[md_links] --head requires a commit reference" >&2; exit 2
      fi
      HEAD_REF="$2"; HEAD_EXPLICIT=1; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "[md_links] Unknown argument: $1" >&2
      usage >&2
      exit 2 ;;
  esac
done

git_has_commit() {
  yield_git rev-parse --verify --quiet "$1^{commit}" >/dev/null 2>&1
}

if [[ -n "$BASE_REF" ]] && ! git_has_commit "$BASE_REF"; then
  echo "[md_links] Invalid base commit: $BASE_REF" >&2; exit 2
fi
if (( HEAD_EXPLICIT )) && ! git_has_commit "$HEAD_REF"; then
  echo "[md_links] Invalid head commit: $HEAD_REF" >&2; exit 2
fi

collect_markdown_files() {
  local base="$1"
  local head="$2"
  # NUL delimiters preserve actual Git paths, including spaces and Unicode.
  if [[ -n "$base" ]] && git_has_commit "$base" && git_has_commit "$head"; then
    yield_git diff --name-only -z --diff-filter=AM "$base...$head" -- '*.md'
  elif git_has_commit "$head"; then
    yield_git show --name-only -z --pretty=format: "$head" -- '*.md'
  fi
  yield_git diff --name-only -z --diff-filter=AM --cached -- '*.md'
  yield_git diff --name-only -z --diff-filter=AM -- '*.md'
}

# The Python source is a file: its stdin is exclusively the actual selected paths.
collect_markdown_files "$BASE_REF" "$HEAD_REF" \
  | python3 "$ROOT_DIR/scripts/internal/check/check_markdown_links.py"
