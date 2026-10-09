#!/usr/bin/env bash
# OPT-2117: run the existing native Legacy worker fixtures without widening their scope.
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Legacy worker verification requires actual native macOS.' >&2
  exit 1
fi
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"
OUTPUT_DIR="${YIELD_NATIVE_VERIFICATION_OUTPUT:-$ROOT_DIR/artifacts/legacy-native-worker}"
NATIVE_TEST_TEMP_ROOT="${TMPDIR:-/tmp}"
mkdir -p "$OUTPUT_DIR"
{
  git rev-parse HEAD
  uname -sm
  sw_vers
  xcodebuild -version
  rustc -Vv
  cargo -V
  printf 'Build jobs: %s\n' "${CARGO_BUILD_JOBS:-host default}"
  sysctl hw.memsize hw.logicalcpu
  df -Pk "$HOME" "$ROOT_DIR" "$NATIVE_TEST_TEMP_ROOT"
  ulimit -a
} > "$OUTPUT_DIR/platform.txt"
cat "$OUTPUT_DIR/platform.txt"
record_filesystem() {
  local location="$1" output="$2" device
  device="$(df -Pk "$location" | awk 'NR == 2 { print $1 }')"
  if [[ "$device" != /dev/disk* ]]; then
    echo "Native filesystem device could not be resolved for $location" >&2
    return 1
  fi
  if ! diskutil info -plist "$device" > "$output"; then
    cat "$output" >&2
    return 1
  fi
}
record_filesystem "$HOME" "$OUTPUT_DIR/home-filesystem.plist"
record_filesystem "$NATIVE_TEST_TEMP_ROOT" "$OUTPUT_DIR/test-filesystem.plist"
python3 - "$OUTPUT_DIR/home-filesystem.plist" "$OUTPUT_DIR/test-filesystem.plist" <<'PY'
import plistlib, sys
for path in sys.argv[1:]:
    with open(path, 'rb') as source:
        facts = plistlib.load(source)
    if facts.get('FilesystemType', '').lower() != 'apfs':
        raise SystemExit('Native Legacy verification requires real APFS home and test volumes')
    if facts.get('VolumeReadOnly', False):
        raise SystemExit('Native Legacy verification requires writable home and test volumes')
print('Native home and test filesystems: APFS, writable')
PY
# These original worker tests use the explicitly isolated native-volume registry.
# They do not qualify default production registry startup or current writable Source handoff.
# Run only one selected case per fresh CI job; preserve dirty evidence inside the test contract.
SELECTED_CASE="${YIELD_NATIVE_LEGACY_CASE:-}"
case "$SELECTED_CASE" in
  operation_journal::owner::legacy_adoption_custody::tests::legacy_initial_native_actual_worker_commit_duplicate_lost_reply_and_source_fence|operation_journal::owner::legacy_adoption_custody::tests::legacy_initial_native_actual_worker_commit_faults_keep_same_source_slot_and_refuse_readmission|operation_journal::owner::legacy_adoption_custody::tests::legacy_migration_native_actual_worker_commits_original_receipt_and_coalesces_lost_reply)
    ;;
  *) echo "Select one existing native Legacy worker case: $SELECTED_CASE" >&2; exit 1 ;;
esac
cargo check --locked -p yield-library --all-targets --features test-support 2>&1 | tee "$OUTPUT_DIR/compile.log"
cargo test --locked -p yield-library --lib "$SELECTED_CASE" \
  -- --exact --test-threads=1 --nocapture 2>&1 | tee "$OUTPUT_DIR/tests.log"
python3 - "$OUTPUT_DIR/tests.log" "$SELECTED_CASE" <<'PY_CHECK'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
selected = sys.argv[2]
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
if results != [('1', '0', '0')]:
    raise SystemExit(f'Expected exactly one selected native test, zero failed/ignored: {results}')
if not re.search(r'test ' + re.escape(selected) + r' \.\.\. ok', text):
    raise SystemExit(f'Missing completed native test: {selected}')
print('OPT-2117: selected isolated-registry native Legacy worker fixture passed')
PY_CHECK
df -Pk "$HOME" "$ROOT_DIR" "$NATIVE_TEST_TEMP_ROOT" > "$OUTPUT_DIR/final-disk.txt"
