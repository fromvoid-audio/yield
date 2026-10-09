#!/usr/bin/env bash
# Resolve a portable Python 3.11+ interpreter for local CI subprocesses.

yield_ci_python() {
  if [[ -n "${YIELD_CI_PYTHON:-}" ]]; then
    if [[ "${YIELD_CI_PYTHON}" == */* ]]; then
      candidate="${YIELD_CI_PYTHON}"
    else
      candidate="$(command -v "${YIELD_CI_PYTHON}" 2>/dev/null || true)"
    fi
    if [[ -n "$candidate" ]] && "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 11))' >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
    echo "YIELD_CI_PYTHON must name Python 3.11 or newer" >&2
    return 1
  fi
  for name in python3 python3.13 python3.12 python3.11; do
    candidate="$(command -v "$name" 2>/dev/null || true)"
    if [[ -n "$candidate" ]] && "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 11))' >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  echo "Yield local CI requires Python 3.11 or newer; install python3.11+ and ensure it is on PATH" >&2
  return 1
}

yield_canonicalize_tmpdir() {
  local canonical helper_dir
  helper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  canonical="$(PYTHONPATH="$helper_dir${PYTHONPATH:+:$PYTHONPATH}" "$YIELD_CI_PYTHON" -c 'from runtime_paths import canonicalize_tmpdir; print(canonicalize_tmpdir())')" || {
    echo "TMPDIR must resolve to an existing directory" >&2
    return 1
  }
  export TMPDIR="$canonical"
}

yield_prepare_python_runtime() {
  local interpreter alias_dir
  interpreter="$(yield_ci_python)" || return 1
  export YIELD_CI_PYTHON="$interpreter"
  yield_canonicalize_tmpdir || return 1
  alias_dir="$(mktemp -d "${TMPDIR:-/tmp}/yield-python.XXXXXX")" || return 1
  chmod 700 "$alias_dir"
  if ! ln -s "$interpreter" "$alias_dir/python3"; then
    rmdir "$alias_dir" 2>/dev/null || true
    return 1
  fi
  export YIELD_CI_PYTHON_RUNTIME_DIR="$alias_dir"
  export PATH="$alias_dir:$PATH"
  trap 'rm -f "$YIELD_CI_PYTHON_RUNTIME_DIR/python3"; rmdir "$YIELD_CI_PYTHON_RUNTIME_DIR" 2>/dev/null || true' EXIT
  if ! python3 -c 'import tomllib' >/dev/null 2>&1; then
    echo "selected Python runtime cannot import tomllib in a python3 child process" >&2
    return 1
  fi
}
