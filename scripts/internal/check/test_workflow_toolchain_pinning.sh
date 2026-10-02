#!/usr/bin/env bash
# Active Rust pinning is enforced; explicitly disabled archives are not executed CI.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts/internal/check"
cp "$root/scripts/internal/check/check_workflow_toolchain_pinning.sh" "$fixture/scripts/internal/check/"
check="$fixture/scripts/internal/check/check_workflow_toolchain_pinning.sh"
expect() {
  local status="$1"
  if "$check" >"$fixture/result" 2>&1; then actual=0; else actual=$?; fi
  if [[ "$actual" != "$status" ]]; then
    cat "$fixture/result" >&2
    echo "[toolchain_test] expected $status got $actual" >&2
    exit 1
  fi
}
expect 1
mkdir -p "$fixture/.github/workflows"
expect 1
printf 'toolchain: stable\n' > "$fixture/.github/workflows/archived.yml.disabled"
expect 0
rg -q 'explicitly disabled.*no CI execution asserted' "$fixture/result"
printf 'uses: dtolnay/rust-toolchain@master\n# derive from rust-toolchain.toml\n' > "$fixture/.github/workflows/current.yaml"
expect 0
printf 'toolchain: stable\n' > "$fixture/.github/workflows/bad.yml"
expect 1
printf 'rustup toolchain install beta\n' > "$fixture/.github/workflows/bad.yml"
expect 1
printf 'uses: actions-rs/toolchain@v1\n' > "$fixture/.github/workflows/bad.yml"
expect 1
printf 'uses: dtolnay/rust-toolchain@master\n' > "$fixture/.github/workflows/bad.yml"
expect 1
rm "$fixture/.github/workflows/bad.yml"
expect 0
echo '[toolchain_test] OK'
