#!/usr/bin/env bash
# Genuine native evidence for OPT-2368, OPT-2378 and OPT-2405.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"
CASE="${YIELD_NATIVE_PREREQUISITE_CASE:?Select a complete native prerequisite case}"
if [[ "$CASE" == startup-namespace-failure ]]; then
  echo 'Use scripts/yield-local-ci check --sha <full-commit-sha> --lane native --case prerequisite:startup-namespace-failure; it runs both receive and poll variants in separate production reset windows.' >&2
  exit 2
fi
HOST="$(rustc -vV | sed -n 's/^host: //p')"
NATIVE_OS="$(uname -s)"
case "$NATIVE_OS:$HOST" in
  Darwin:x86_64-apple-darwin|Darwin:aarch64-apple-darwin) PLATFORM=macos ;;
  MINGW*:x86_64-pc-windows-msvc|MSYS*:x86_64-pc-windows-msvc) PLATFORM=windows ;;
  *) echo "Requires genuine macOS or Windows/MSVC execution: $NATIVE_OS:$HOST" >&2; exit 1 ;;
esac
OUTPUT_DIR="${YIELD_NATIVE_VERIFICATION_OUTPUT:-$ROOT_DIR/artifacts/native-prerequisites/$PLATFORM-$CASE}"
mkdir -p "$OUTPUT_DIR"
{
  git rev-parse HEAD
  uname -a
  id
  rustc -Vv
  cargo -V
  printf 'Native case: %s\nHost: %s\nBuild jobs: %s\n' "$CASE" "$HOST" "${CARGO_BUILD_JOBS:-host default}"
  df -Pk "$HOME" "$ROOT_DIR" "${TMPDIR:-/tmp}"
  ulimit -a
  if [[ "$PLATFORM" == macos ]]; then
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
    sysctl hw.memsize hw.logicalcpu
  else
    powershell.exe -NoProfile -Command 'Get-CimInstance Win32_OperatingSystem | Format-List Caption,Version,BuildNumber; Get-Volume | Format-Table DriveLetter,FileSystem,SizeRemaining,Size; whoami /user; Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Include" -Directory | Select-Object Name'
  fi
} > "$OUTPUT_DIR/platform.txt"
cat "$OUTPUT_DIR/platform.txt"
if [[ "$PLATFORM" == macos ]]; then
  for location in "$HOME" "${TMPDIR:-/tmp}"; do
    device="$(df -Pk "$location" | awk 'NR == 2 { print $1 }')"
    [[ "$device" == /dev/disk* ]] || { echo 'Unresolved native device' >&2; exit 1; }
    diskutil info -plist "$device" > "$OUTPUT_DIR/filesystem-$(basename "$device").plist"
  done
  python3 - "$OUTPUT_DIR" <<'PY'
import pathlib, plistlib, sys
paths = list(pathlib.Path(sys.argv[1]).glob('filesystem-*.plist'))
if not paths:
    raise SystemExit('Missing original native filesystem evidence')
for path in paths:
    facts = plistlib.loads(path.read_bytes())
    if facts.get('FilesystemType', '').lower() != 'apfs' or facts.get('VolumeReadOnly', False):
        raise SystemExit('Native verification requires genuine writable APFS')
PY
fi
run_tests() {
  local label="$1" selector="$2" expected="$3"
  shift 3
  printf 'cargo test --locked -p yield-library --lib %q -- --test-threads=1 --nocapture' "$selector" | tee "$OUTPUT_DIR/$label.command"
  printf ' %q' "$@" | tee -a "$OUTPUT_DIR/$label.command"
  printf '\n' | tee -a "$OUTPUT_DIR/$label.command"
  cargo test --locked -p yield-library --lib "$selector" -- --test-threads=1 --nocapture "$@" 2>&1 | tee "$OUTPUT_DIR/$label.log"
  python3 - "$OUTPUT_DIR/$label.log" "$expected" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
if not results:
    raise SystemExit('Missing successful executed native tests')
passed, failed, ignored = map(int, results[-1])
expected = int(sys.argv[2])
if failed or not passed or (expected and (passed != expected or ignored)):
    raise SystemExit(f'Invalid native result: passed={passed}, failed={failed}, ignored={ignored}, expected={expected or "nonzero"}')
print(f'Checked native result: {passed} passed, {failed} failed, {ignored} ignored')
PY
}
case "$CASE" in
  profile-generations)
    # The profile suite intentionally ignores its standalone child-process helper;
    # passing parent tests must actually launch/reap it on this native kernel.
    run_tests profile app_dirs::profile_ownership 0
    run_tests generations profile_generation 0
    ;;
  source-release-policy)
    if [[ "$PLATFORM" == macos ]]; then
      run_tests native-shm darwin_original_shared_mapping_accepts_single_reference_and_refuses_cow 1
    fi
    cargo test --locked -p yield-library --lib source_retirement_policy -- --list > "$OUTPUT_DIR/source-policy-list.txt"
    run_tests source-policy source_retirement_policy 2
    run_tests generic-compatibility generic_registered_lease_preparation_retains_no_retirement_policy 1
    run_tests arithmetic source_release_component 2
    if [[ "$PLATFORM" == macos ]]; then
      run_tests all-owner all_owner_real_registry_retirement_collects_global_and_pre_audit_lease_with_exact_graph 1
      run_tests missing-policy retirement_original_consumer_refuses_missing_source_release_policy 1
      run_tests foreign-family retirement_original_consumer_refuses_another_valid_source_release_family 1
    fi
    ;;
  source-registration-generation)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests registration-generation registration_generation 7
    run_tests retained-source-rollback preaudit_denied_rollback_retry_retains_registered_count_and_original_policy 1
    run_tests foreign-owner-decision genuine_foreign_source_registration_cannot_fill_original_decision_slot 1
    ;;
  startup-production)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests production operation_journal::owner::tests::capacity_startup_tests::capacity_startup_native_production_rebuilds_original_descriptor_bound_image 1 --exact
    ;;
  startup-control-failure)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests production-control operation_journal::owner::tests::control_backing_tests::control_backing_native_production_startup_preserves_original_failure_and_journal 1 --exact
    ;;
  startup-namespace-failure)
    echo 'Use the managed CLI aggregate for both namespace failure receive/poll variants.' >&2
    exit 2
    ;;
  startup-namespace-receive-failure)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests production-namespace-receive operation_journal::owner::tests::namespace_backing_tests::namespace_backing_native_production_error_outlives_original_worker_join 1 --exact
    ;;
  startup-namespace-poll-failure)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests production-namespace-poll operation_journal::owner::tests::namespace_backing_tests::namespace_backing_native_production_error_outlives_original_worker_join_poll 1 --exact
    ;;
  sdk-and-startup-components)
    [[ "$PLATFORM" == macos ]] || exit 1
    run_tests startup-components capacity_startup_tests 1 --skip capacity_startup_native_production
    run_tests detached-credit operation_journal::owner::startup_credit::tests 0
    run_tests provider-startup startup 0 --skip native_production --skip queued_native --skip namespace_backing
    run_tests rebuild-conflict owner_runtime_rebuild_reuses_live_provider_on_conflicting_snapshot 1
    run_tests proof-failure owner_runtime_does_not_read_or_install_after_physical_proof_failure 1
    run_tests reserve-bootstrap owner_bootstraps_and_reopens_profile_journal_reserve 1
    bash scripts/internal/agent/run_agent_preflight.sh 2>&1 | tee "$OUTPUT_DIR/canonical.log"
    ;;
  *) echo "Unknown native prerequisite case: $CASE" >&2; exit 1 ;;
esac
# This is a supported-host compiler/linker check, distinct from each genuine run.
cargo check --locked -p yield-library --all-targets --features test-support 2>&1 | tee "$OUTPUT_DIR/sdk-check.log"
printf 'Native prerequisite case passed: %s %s\n' "$PLATFORM" "$CASE"
