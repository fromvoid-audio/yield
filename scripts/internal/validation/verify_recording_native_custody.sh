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
diskutil info -plist "$HOME" > "$OUTPUT_DIR/home-filesystem.plist"
diskutil info -plist "$NATIVE_TEST_TEMP_ROOT" > "$OUTPUT_DIR/test-filesystem.plist"
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
cargo test --locked -p yield-library --lib recording_custody_native_production \
  -- --test-threads=1 --nocapture 2>&1 | tee "$OUTPUT_DIR/tests.log"
python3 - "$OUTPUT_DIR/tests.log" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
if results != [('2', '0', '0')]:
    raise SystemExit(f'Expected exactly two executed genuine tests, zero failed/ignored: {results}')
for name in (
    'recording_custody_native_production_original_slot_currentness_and_drain',
    'recording_custody_native_production_lost_reply_keeps_original_slot_and_credit',
):
    if not re.search(r'test [\w:]*' + re.escape(name) + r' \.\.\. ok', text):
        raise SystemExit(f'Missing completed genuine test: {name}')
print('OPT-2330: exactly two genuine production-bootstrap tests passed')
PY
df -Pk "$HOME" "$ROOT_DIR" "$NATIVE_TEST_TEMP_ROOT" > "$OUTPUT_DIR/final-disk.txt"
