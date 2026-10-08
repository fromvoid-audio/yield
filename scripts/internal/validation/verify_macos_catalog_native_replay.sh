#!/usr/bin/env bash
# OPT-2080: actual native callback/catalog execution, never portable producer qualification.
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Native replay acceptance requires actual macOS.' >&2
  exit 1
fi
REPLAY_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPLAY_REPO_ROOT"
REPLAY_EVIDENCE_DIR="$REPLAY_REPO_ROOT/artifacts/macos-catalog-native-replay"
mkdir -p "$REPLAY_EVIDENCE_DIR"
{
  git rev-parse HEAD
  uname -sm
  sw_vers
  xcodebuild -version
  rustc -Vv
  cargo -V
  df -Pk "$REPLAY_REPO_ROOT" "${TMPDIR:-/tmp}"
} > "$REPLAY_EVIDENCE_DIR/platform.txt"
cat "$REPLAY_EVIDENCE_DIR/platform.txt"
REPLAY_TEST_DEVICE="$(df -Pk "${TMPDIR:-/tmp}" | awk 'NR == 2 { print $1 }')"
if [[ "$REPLAY_TEST_DEVICE" != /dev/disk* ]]; then
  echo 'Actual native test filesystem unavailable.' >&2
  exit 1
fi
diskutil info -plist "$REPLAY_TEST_DEVICE" > "$REPLAY_EVIDENCE_DIR/test-filesystem.plist"
python3 - "$REPLAY_EVIDENCE_DIR/test-filesystem.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], 'rb') as source:
    facts = plistlib.load(source)
if facts.get('FilesystemType', '').lower() != 'apfs' or facts.get('VolumeReadOnly', False):
    raise SystemExit('This native lane requires actual writable APFS test storage')
PY
run_replay_case() {
  local replay_package="$1" replay_filter="$2" replay_count="$3" replay_log="$4"
  cargo test --locked -p "$replay_package" --lib "$replay_filter" -- --test-threads=1 --nocapture \
    2>&1 | tee "$REPLAY_EVIDENCE_DIR/$replay_log"
  python3 - "$REPLAY_EVIDENCE_DIR/$replay_log" "$replay_count" <<'PY'
import pathlib, re, sys
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', pathlib.Path(sys.argv[1]).read_text())
if results != [(sys.argv[2], '0', '0')]:
    raise SystemExit(f'Expected {sys.argv[2]} actual tests, zero failure/ignore: {results}')
PY
}
run_replay_case yield-source-watcher captures_native_history_for_a_disposable_source 1 native-history.log
run_replay_case yield-source-watcher live_bootstrap_capture_starts_before_audit_and_flushes_changes 1 native-bootstrap.log
run_replay_case yield-source-watcher live_bootstrap_capture_rejects_root_replacement 1 native-root-replacement.log
run_replay_case yield-catalog native_initial_audit_bootstraps_durable_replay_prior 1 catalog-bootstrap.log
run_replay_case yield-catalog canceled_native_initial_audit_leaves_no_replay_prior 1 catalog-canceled.log
run_replay_case yield-catalog native_watcher_hint_commits_bounded_replay_delta 1 native-controller.log
run_replay_case yield-catalog replay:: 19 catalog-replay.log
run_replay_case yield-ui browser::catalog::tests::replay_projection 2 ui-delta.log
bash scripts/internal/agent/run_agent_preflight.sh 2>&1 | tee "$REPLAY_EVIDENCE_DIR/preflight.log"
cargo check --locked -p yield-source-watcher -p yield-catalog -p yield-library -p yield-ui -p yield \
  2>&1 | tee "$REPLAY_EVIDENCE_DIR/native-sdk.log"
# Finder overflow/restart, GPUI interaction and Source physical-family acceptance remain manual.
