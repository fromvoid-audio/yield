#!/usr/bin/env bash

# Guards against workflow/toolchain drift by ensuring workflows that install
# Rust derive the toolchain from `rust-toolchain.toml` (not "stable").

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"

WORKFLOWS_DIR=".github/workflows"

if [[ ! -d "$WORKFLOWS_DIR" ]]; then
  echo "[toolchain_pinning] Missing $WORKFLOWS_DIR" >&2
  exit 1
fi

shopt -s nullglob
files=("$WORKFLOWS_DIR"/*.yml "$WORKFLOWS_DIR"/*.yaml)
if (( ${#files[@]} == 0 )); then
  archived=("$WORKFLOWS_DIR"/*.yml.disabled "$WORKFLOWS_DIR"/*.yaml.disabled)
  if (( ${#archived[@]} == 0 )); then
    echo "[toolchain_pinning] No active or explicitly disabled workflows found" >&2
    exit 1
  fi
  echo "[toolchain_pinning] ${#archived[@]} explicitly disabled workflows; no CI execution asserted"
  exit 0
fi

violations=0
for f in "${files[@]}"; do
  if rg -q "dtolnay/rust-toolchain@" "$f"; then
    if ! rg -q "rust-toolchain\\.toml" "$f"; then
      if (( violations == 0 )); then
        echo "[toolchain_pinning] Workflows must derive toolchain from rust-toolchain.toml:" >&2
      fi
      echo " - $f: uses dtolnay/rust-toolchain but does not reference rust-toolchain.toml" >&2
      violations=$((violations + 1))
    fi
  fi

  # Forbid literal stable/beta/nightly toolchain installs (action tags like @stable are OK).
  if rg -q '^[[:space:]]*toolchain:[[:space:]]*["'"'"']?(stable|beta|nightly)["'"'"']?[[:space:]]*$' "$f"; then
    if (( violations == 0 )); then
      echo "[toolchain_pinning] Workflows must not set toolchain: stable/beta/nightly literals:" >&2
    fi
    echo " - $f: contains literal toolchain: stable/beta/nightly (use rust-toolchain.toml)" >&2
    violations=$((violations + 1))
  fi

  if rg -q 'rustup[[:space:]]+toolchain[[:space:]]+install[[:space:]]+(stable|beta|nightly)\b' "$f"; then
    if (( violations == 0 )); then
      echo "[toolchain_pinning] Workflows must not install toolchain by name:" >&2
    fi
    echo " - $f: installs toolchain by name (use rust-toolchain.toml)" >&2
    violations=$((violations + 1))
  fi

  if rg -q "actions-rs/toolchain" "$f"; then
    if (( violations == 0 )); then
      echo "[toolchain_pinning] Prefer dtolnay/rust-toolchain + rust-toolchain.toml; do not use actions-rs/toolchain:" >&2
    fi
    echo " - $f: uses actions-rs/toolchain" >&2
    violations=$((violations + 1))
  fi
done

if (( violations > 0 )); then
  exit 1
fi

echo "[toolchain_pinning] OK"
exit 0

