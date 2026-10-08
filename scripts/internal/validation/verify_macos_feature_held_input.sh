#!/usr/bin/env bash
# OPT-2061: actual Darwin descriptor/Source feature execution, never a Linux substitute.
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Held feature input acceptance requires actual native macOS.' >&2
  exit 1
fi
FEATURE_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$FEATURE_REPO_ROOT"
FEATURE_EVIDENCE_DIR="$FEATURE_REPO_ROOT/artifacts/macos-feature-held-input"
mkdir -p "$FEATURE_EVIDENCE_DIR"
{
  git rev-parse HEAD
  uname -sm
  sw_vers
  xcodebuild -version
  rustc -Vv
  cargo -V
  df -Pk "$FEATURE_REPO_ROOT" "${TMPDIR:-/tmp}"
} > "$FEATURE_EVIDENCE_DIR/platform.txt"
cat "$FEATURE_EVIDENCE_DIR/platform.txt"
FEATURE_TEST_DEVICE="$(df -Pk "${TMPDIR:-/tmp}" | awk 'NR == 2 { print $1 }')"
if [[ "$FEATURE_TEST_DEVICE" != /dev/disk* ]]; then
  echo 'Actual test filesystem device unavailable.' >&2
  exit 1
fi
diskutil info -plist "$FEATURE_TEST_DEVICE" > "$FEATURE_EVIDENCE_DIR/test-filesystem.plist"
python3 - "$FEATURE_EVIDENCE_DIR/test-filesystem.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], 'rb') as source:
    facts = plistlib.load(source)
if facts.get('FilesystemType', '').lower() != 'apfs' or facts.get('VolumeReadOnly', False):
    raise SystemExit('This acceptance lane requires actual writable APFS test storage')
PY
cargo test --locked -p yield-library --lib index_inspection_primitive_tests \
  -- --test-threads=1 --nocapture 2>&1 | tee "$FEATURE_EVIDENCE_DIR/held-index.log"
cargo test --locked -p yield-catalog --lib tests::feature_job_tests::fixtures \
  -- --test-threads=1 --nocapture 2>&1 | tee "$FEATURE_EVIDENCE_DIR/feature-jobs.log"
python3 - "$FEATURE_EVIDENCE_DIR" <<'PY'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
for name, count in [('held-index.log', 3), ('feature-jobs.log', 11)]:
    text = (root / name).read_text()
    results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
    if results != [(str(count), '0', '0')]:
        raise SystemExit(f'Expected {count} actual tests, zero failed/ignored in {name}: {results}')
PY
cargo test --locked -p yield-library --lib feature_artifact -- --test-threads=1 \
  2>&1 | tee "$FEATURE_EVIDENCE_DIR/feature-artifact.log"
cargo test --locked -p yield-library --lib readiness -- --test-threads=1 \
  2>&1 | tee "$FEATURE_EVIDENCE_DIR/readiness.log"
cargo test --locked -p yield-analysis --lib 2>&1 | tee "$FEATURE_EVIDENCE_DIR/analysis.log"
python3 - "$FEATURE_EVIDENCE_DIR" <<'PY'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
for name in ['feature-artifact.log', 'readiness.log', 'analysis.log']:
    results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', (root / name).read_text())
    if len(results) != 1 or int(results[0][0]) == 0 or results[0][1:] != ('0', '0'):
        raise SystemExit(f'Missing actual zero-failure/zero-ignore tests in {name}: {results}')
PY
bash scripts/internal/agent/run_agent_preflight.sh 2>&1 | tee "$FEATURE_EVIDENCE_DIR/preflight.log"
cargo check --locked -p yield-library -p yield-catalog -p yield-analysis -p yield-ui -p yield \
  2>&1 | tee "$FEATURE_EVIDENCE_DIR/native-sdk.log"
