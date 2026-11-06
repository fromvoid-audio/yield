#!/usr/bin/env bash
# Opt-in CoreAudio audition acceptance for the real native callback path.
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'CoreAudio audition acceptance requires actual macOS.' >&2
  exit 1
fi
if [[ "${YIELD_TEST_AUDIO_OUTPUT:-}" != 1 ]]; then
  echo 'Set YIELD_TEST_AUDIO_OUTPUT=1 explicitly to opt in to real audio output.' >&2
  exit 1
fi

AUDITION_REPO_ROOT="${YIELD_GPUI_CI_CHECKOUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)}"
cd "$AUDITION_REPO_ROOT"
AUDITION_EVIDENCE_DIR="${YIELD_NATIVE_VERIFICATION_OUTPUT:-$AUDITION_REPO_ROOT/artifacts/macos-audition-coreaudio}"
mkdir -p "$AUDITION_EVIDENCE_DIR"

{
  printf 'source_sha=%s\n' "${YIELD_LOCAL_CI_SOURCE_SHA:-unknown}"
  uname -sm
  sw_vers
  developer_dir="$(xcode-select -p)"
  printf 'Developer directory: %s\n' "$developer_dir"
  if [[ "$developer_dir" == */CommandLineTools ]]; then
    pkgutil --pkg-info com.apple.pkg.CLTools_Executables
  else
    xcodebuild -version
  fi
  xcrun --sdk macosx --show-sdk-path
  xcrun --sdk macosx --show-sdk-version
  xcrun --sdk macosx clang --version
  rustc -Vv
  cargo -V
  df -Pk "$AUDITION_REPO_ROOT" "${TMPDIR:-/tmp}"
  system_profiler SPAudioDataType
} > "$AUDITION_EVIDENCE_DIR/platform-and-audio-devices.txt"
cat "$AUDITION_EVIDENCE_DIR/platform-and-audio-devices.txt"

AUDITION_TEST_DEVICE="$(df -Pk "${TMPDIR:-/tmp}" | awk 'NR == 2 { print $1 }')"
if [[ "$AUDITION_TEST_DEVICE" != /dev/disk* ]]; then
  echo 'Native audition evidence requires an actual local disk device.' >&2
  exit 1
fi
diskutil info -plist "$AUDITION_TEST_DEVICE" > "$AUDITION_EVIDENCE_DIR/test-filesystem.plist"
python3 - "$AUDITION_EVIDENCE_DIR/test-filesystem.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], 'rb') as source:
    facts = plistlib.load(source)
filesystem = str(facts.get('FilesystemType', '')).lower()
writable = facts.get('WritableVolume') is True or facts.get('ReadOnlyVolume') is False
if filesystem != 'apfs' or not writable:
    raise SystemExit('CoreAudio audition acceptance requires actual writable APFS storage')
PY

run_selector() {
  local selector="$1"
  local label="$2"
  local log_prefix="$AUDITION_EVIDENCE_DIR/$label"
  local status
  local command=(cargo test --locked -p reson --features native-audio --lib "$selector" -- --exact --ignored --test-threads=1 --nocapture)

  printf '$'
  printf ' %q' "${command[@]}"
  printf '\n'
  set +e
  "${command[@]}" > "$log_prefix.stdout.log" 2> "$log_prefix.stderr.log"
  status=$?
  set -e
  cat "$log_prefix.stdout.log"
  if [[ -s "$log_prefix.stderr.log" ]]; then
    printf '[native stderr; kept separate from the libtest transcript]\n'
    cat "$log_prefix.stderr.log"
  fi
  if ! python3 - "$selector" "$status" "$log_prefix.stdout.log" "$log_prefix.result.json" <<'PY'
import json, pathlib, re, sys, time
selector, raw_status, log_path, result_path = sys.argv[1:]
status = int(raw_status)
text = pathlib.Path(log_path).read_text(encoding='utf-8', errors='replace')
running = re.findall(r'(?m)^running (\d+) tests?$', text)
executed = re.findall(r'(?m)^test (.+?) \.\.\. (ok|FAILED)$', text)
summary = re.findall(
    r'(?m)^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in ([^\n]+)$',
    text,
)
expected_execution = [(selector, 'ok')]
valid = (
    status == 0
    and running == ['1']
    and executed == expected_execution
    and len(summary) == 1
    and summary[0][0:4] == ('ok', '1', '0', '0')
)
record = {
    'schema': 1,
    'selector': selector,
    'command_exit_code': status,
    'running_counts': running,
    'executed_tests': [{'selector': name, 'result': result} for name, result in executed],
    'summaries': [list(item) for item in summary],
    'exactly_one_success_verified': valid,
    'completed_unix_ns': time.time_ns(),
}
pathlib.Path(result_path).write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')
if not valid:
    raise SystemExit(f'exact selector/count contract failed; see {result_path}')
PY
  then
    return 1
  fi
}

failures=0
run_selector \
  player::runtime::tests::macos_real_executor_applies_anti_click_settings_independently_of_edit_fades \
  anti-click-settings || failures=1
run_selector \
  player::runtime::tests::macos_private_aggregate_routes_nonzero_pair_through_native_callback \
  private-aggregate-routing || failures=1
if (( failures != 0 )); then
  exit 1
fi
