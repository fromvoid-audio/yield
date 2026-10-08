#!/usr/bin/env bash
# OPT-2330: execute the existing genuine production-bootstrap tests on macOS.
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Recording custody qualification requires actual native macOS.' >&2
  exit 1
fi
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"
OUTPUT_DIR="${YIELD_NATIVE_VERIFICATION_OUTPUT:-$ROOT_DIR/artifacts/recording-native-custody}"
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
        raise SystemExit('Native recording verification requires real APFS home and test volumes')
    if facts.get('VolumeReadOnly', False):
        raise SystemExit('Native recording verification requires writable home and test volumes')
print('Native home and test filesystems: APFS, writable')
PY
# An admitted prestart intent leaves genuine registration/charge evidence. A
# joined worker does not establish clean volume handoff. CI runs each case on a
# separate fresh native account, preserving the original dirty marker unchanged.
SELECTED_CASE="${YIELD_NATIVE_CUSTODY_CASE:-}"
case "$SELECTED_CASE" in
  operation_journal::owner::recording_binding::tests::recording_custody_native_production_original_slot_currentness_and_drain|operation_journal::owner::recording_binding::tests::recording_custody_native_production_lost_reply_keeps_original_slot_and_credit)
    cargo test --locked -p yield-library --lib "$SELECTED_CASE" \
      -- --exact --test-threads=1 --nocapture 2>&1 | tee "$OUTPUT_DIR/tests.log"
    ;;
  '')
    cargo test --locked -p yield-library --lib recording_custody_native_production \
      -- --test-threads=1 --nocapture 2>&1 | tee "$OUTPUT_DIR/tests.log"
    ;;
  *) echo "Unknown genuine custody case: $SELECTED_CASE" >&2; exit 1 ;;
esac
python3 - "$OUTPUT_DIR/tests.log" "$SELECTED_CASE" <<'PY_CHECK'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
selected = sys.argv[2]
names = (selected,) if selected else (
    'operation_journal::owner::recording_binding::tests::recording_custody_native_production_original_slot_currentness_and_drain',
    'operation_journal::owner::recording_binding::tests::recording_custody_native_production_lost_reply_keeps_original_slot_and_credit',
)
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
if results != [(str(len(names)), '0', '0')]:
    raise SystemExit(f'Expected all {len(names)} selected genuine tests, zero failed/ignored: {results}')
for name in names:
    if not re.search(r'test ' + re.escape(name) + r' \.\.\. ok', text):
        raise SystemExit(f'Missing completed genuine test: {name}')
print(f'OPT-2330: {len(names)} selected genuine production-bootstrap test(s) passed')
PY_CHECK
df -Pk "$HOME" "$ROOT_DIR" "$NATIVE_TEST_TEMP_ROOT" > "$OUTPUT_DIR/final-disk.txt"
