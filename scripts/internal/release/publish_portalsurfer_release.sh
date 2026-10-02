#!/usr/bin/env bash
set -euo pipefail

ARTIFACT_DIR="dist/release"
RELEASE_LOG="dist/release/release-log.md"
FULL_CHANGELOG_OUT="dist/release/changelog.md"
CHANNEL=""
BUILD_ID=""
BUILD_NUMBER=""
RELEASE_VERSION=""
RELEASED_AT=""
FULL_COMMIT=""

usage() {
  cat <<'USAGE'
Usage: publish_portalsurfer_release.sh --channel <nightly|rc|stable> --build-id <id> --build-number <n> --release-version <version> --released-at <iso-time> [--artifact-dir <path>] [--release-log <path>] [--full-changelog-out <path>]

Requires PORTALSURFER_RELEASE_UPLOAD_TOKEN. Uses PORTALSURFER_RELEASE_UPLOAD_URL
when set, otherwise https://portalsurfer.org/yield/api/v1/release-uploads.
Validates all contract ZIPs/checksums and derives full commit from their manifests
before uploading a private snapshot. Server responses must echo complete identity.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --channel)
      CHANNEL="${2:-}"
      shift 2
      ;;
    --build-id)
      BUILD_ID="${2:-}"
      shift 2
      ;;
    --build-number)
      BUILD_NUMBER="${2:-}"
      shift 2
      ;;
    --release-version)
      RELEASE_VERSION="${2:-}"
      shift 2
      ;;
    --released-at)
      RELEASED_AT="${2:-}"
      shift 2
      ;;
    --artifact-dir)
      ARTIFACT_DIR="${2:-}"
      shift 2
      ;;
    --release-log)
      RELEASE_LOG="${2:-}"
      shift 2
      ;;
    --full-changelog-out)
      FULL_CHANGELOG_OUT="${2:-}"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$CHANNEL" || -z "$BUILD_ID" || -z "$BUILD_NUMBER" || -z "$RELEASE_VERSION" || -z "$RELEASED_AT" ]]; then
  usage >&2
  exit 1
fi
case "$CHANNEL" in
  nightly|rc|stable) ;;
  *)
    echo "channel must be nightly, rc, or stable." >&2
    exit 1
    ;;
esac
if [[ -z "${PORTALSURFER_RELEASE_UPLOAD_TOKEN:-}" ]]; then
  echo "Missing required secret: PORTALSURFER_RELEASE_UPLOAD_TOKEN" >&2
  exit 1
fi
if [[ ! "$BUILD_NUMBER" =~ ^[0-9]+$ ]]; then
  echo "build-number must be numeric." >&2
  exit 1
fi
if [[ ! -s "$RELEASE_LOG" ]]; then
  echo "Release log is missing or empty: $RELEASE_LOG" >&2
  exit 1
fi

PORTALSURFER_RELEASE_CONNECT_TIMEOUT_SECONDS="${PORTALSURFER_RELEASE_CONNECT_TIMEOUT_SECONDS:-10}"
PORTALSURFER_RELEASE_MAX_TIME_SECONDS="${PORTALSURFER_RELEASE_MAX_TIME_SECONDS:-300}"
for timeout_value in "$PORTALSURFER_RELEASE_CONNECT_TIMEOUT_SECONDS" "$PORTALSURFER_RELEASE_MAX_TIME_SECONDS"; do
  if [[ ! "$timeout_value" =~ ^[1-9][0-9]*$ ]]; then
    echo "PortalSurfer curl timeout values must be positive integer seconds." >&2
    exit 1
  fi
done
curl_args=(
  --fail-with-body
  --retry 3
  --retry-delay 2
  --connect-timeout "$PORTALSURFER_RELEASE_CONNECT_TIMEOUT_SECONDS"
  --max-time "$PORTALSURFER_RELEASE_MAX_TIME_SECONDS"
  --max-filesize 524288
)

upload_base="${PORTALSURFER_RELEASE_UPLOAD_URL:-https://portalsurfer.org/yield/api/v1/release-uploads}"
upload_base="${upload_base%/}"
if [[ "$upload_base" != */release-uploads ]]; then
  echo "PORTALSURFER_RELEASE_UPLOAD_URL must end with /release-uploads." >&2
  exit 1
fi

api_base="${upload_base%/release-uploads}"
catalog_url="$api_base/releases"
full_changelog_url="$api_base/changelog"
response_dir="$(mktemp -d)"
staged_files_tsv="$response_dir/staged-files.tsv"
: > "$staged_files_tsv"
summary_phase="validating release artifacts"
summary_written=0

write_portalsurfer_summary() {
  local status_code="$1"
  if [[ "$summary_written" == "1" ]]; then
    return 0
  fi
  summary_written=1
  local status_text="success"
  if [[ "$status_code" != "0" ]]; then
    status_text="failure during ${summary_phase}"
  fi
  local summary_artifact_dir="$ARTIFACT_DIR"
  if [[ -d "${snapshot_dir:-}" ]]; then
    summary_artifact_dir="$snapshot_dir"
  fi
  local checksum_file=""
  shopt -s nullglob
  local checksum_candidates=("${summary_artifact_dir}"/checksums-*.txt)
  shopt -u nullglob
  if [[ ${#checksum_candidates[@]} -gt 0 ]]; then
    checksum_file="${checksum_candidates[0]}"
  fi
  bash scripts/internal/release/write_release_step_summary.sh \
    --title "PortalSurfer ${CHANNEL} publication" \
    --status "$status_text" \
    --channel "$CHANNEL" \
    --version "$RELEASE_VERSION" \
    --commit "$FULL_COMMIT" \
    --build-id "$BUILD_ID" \
    --build-number "$BUILD_NUMBER" \
    --portal-catalog-url "$catalog_url" \
    --portal-build-id "$BUILD_ID" \
    --artifact-dir "$summary_artifact_dir" \
    --checksum-file "$checksum_file" \
    --note "PortalSurfer phase: $summary_phase" || true
}

cleanup() {
  local status_code="$?"
  write_portalsurfer_summary "$status_code"
  rm -rf "$response_dir"
  exit "$status_code"
}
trap cleanup EXIT

identity_file="$response_dir/packaged-identity.json"
snapshot_dir="$response_dir/artifacts"
FULL_COMMIT="$(python3 scripts/internal/release/publisher_identity.py prepare \
  --artifact-dir "$ARTIFACT_DIR" --snapshot-dir "$snapshot_dir" \
  --identity-file "$identity_file" --channel "$CHANNEL" --build-id "$BUILD_ID" \
  --build-number "$BUILD_NUMBER" --version "$RELEASE_VERSION" --released-at "$RELEASED_AT")"
files=("$snapshot_dir"/*)

summary_phase="staging release files"
for file in "${files[@]}"; do
  file_name="$(basename "$file")"
  if [[ ! -s "$file" ]]; then
    echo "Release file is empty: $file" >&2
    exit 1
  fi
  content_type="application/octet-stream"
  if [[ "$file_name" == *.zip ]]; then
    content_type="application/zip"
  fi
  sha256="$(sha256sum "$file" | awk '{ print $1 }')"
  size_bytes="$(wc -c < "$file" | tr -d '[:space:]')"
  encoded_name="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$file_name")"
  response_file="$response_dir/$file_name.json"
  echo "Staging PortalSurfer release file: $file_name" >&2
  curl "${curl_args[@]}" \
    --request PUT \
    --header "Authorization: Bearer $PORTALSURFER_RELEASE_UPLOAD_TOKEN" \
    --header "Content-Type: $content_type" \
    --header "X-Yield-Build-Number: ${BUILD_NUMBER}" \
    --header "X-Yield-Release-Channel: $CHANNEL" \
    --header "X-Yield-Release-Version: $RELEASE_VERSION" \
    --header "X-Yield-Released-At: $RELEASED_AT" \
    --header "X-Yield-Commit: $FULL_COMMIT" \
    --header "X-Yield-Sha256: $sha256" \
    --data-binary "@$file" \
    "$upload_base/$BUILD_ID/staging/files/$encoded_name" > "$response_file"
  python3 scripts/internal/release/publisher_identity.py verify \
    --identity-file "$identity_file" --response-file "$response_file" \
    --phase staging --file-name "$file_name"
  printf "%s\t%s\t%s\n" "$file_name" "$sha256" "$size_bytes" >> "$staged_files_tsv"
done

summary_phase="assembling full changelog"
assembly_catalog_file="$response_dir/assembly-catalog.json"
assembly_changelog_file="$response_dir/assembly-changelog.json"
curl "${curl_args[@]}" "$catalog_url" > "$assembly_catalog_file"
curl "${curl_args[@]}" "$full_changelog_url" > "$assembly_changelog_file"
scripts/internal/release/assemble_portal_changelog.py \
  --catalog-file "$assembly_catalog_file" \
  --current-build-id "$BUILD_ID" \
  --current-build-number "$BUILD_NUMBER" \
  --current-version "$RELEASE_VERSION" \
  --current-released-at "$RELEASED_AT" \
  --current-log "$RELEASE_LOG" \
  --existing-changelog-file "$assembly_changelog_file" \
  --generated-at "$RELEASED_AT" \
  --output "$FULL_CHANGELOG_OUT"

commit_request_file="$response_dir/release-commit.json"
python3 - "$staged_files_tsv" "$RELEASE_LOG" "$FULL_CHANGELOG_OUT" "$RELEASED_AT" "$FULL_COMMIT" "$commit_request_file" <<'PY'
import json
import sys

files_path, release_log_path, full_changelog_path, released_at, full_commit, out_path = sys.argv[1:]
files = []
with open(files_path, encoding="utf-8") as handle:
    for line in handle:
        name, sha256, size_bytes = line.rstrip("\n").split("\t")
        files.append({"name": name, "sha256": sha256, "size_bytes": int(size_bytes)})
with open(release_log_path, encoding="utf-8") as handle:
    release_log = handle.read()
with open(full_changelog_path, encoding="utf-8") as handle:
    full_changelog = handle.read()
body = {
    "commit": full_commit,
    "files": files,
    "changelog": {
        "body": release_log,
        "generated_at": released_at,
    },
    "full_changelog": {
        "title": "Yield changelog",
        "body": full_changelog,
        "generated_at": released_at,
    },
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(body, handle)
PY

commit_response_file="$response_dir/release-commit-response.json"
echo "Committing PortalSurfer release: $BUILD_ID" >&2
summary_phase="committing staged release"
curl "${curl_args[@]}" \
  --request PUT \
  --header "Authorization: Bearer $PORTALSURFER_RELEASE_UPLOAD_TOKEN" \
  --header "Content-Type: application/json" \
  --header "X-Yield-Build-Number: ${BUILD_NUMBER}" \
  --header "X-Yield-Release-Channel: $CHANNEL" \
  --header "X-Yield-Release-Version: $RELEASE_VERSION" \
  --header "X-Yield-Released-At: $RELEASED_AT" \
  --header "X-Yield-Commit: $FULL_COMMIT" \
  --data-binary "@$commit_request_file" \
  "$upload_base/$BUILD_ID/commit" > "$commit_response_file"
python3 scripts/internal/release/publisher_identity.py verify \
  --identity-file "$identity_file" --response-file "$commit_response_file" --phase commit
python3 - "$commit_response_file" "$BUILD_ID" "$BUILD_NUMBER" <<'PY'
import json
import sys

response_path, expected_build, expected_build_number = sys.argv[1:]
response = json.loads(open(response_path, encoding="utf-8").read())
release = response.get("release") or {}
changelog = response.get("changelog") or {}
full_changelog = response.get("full_changelog") or {}
if response.get("committed") is not True:
    raise SystemExit("PortalSurfer did not report a committed release")
if release.get("build_id") != expected_build:
    raise SystemExit(f"Committed release id mismatch: {release.get('build_id')} != {expected_build}")
if release.get("build_number") != int(expected_build_number):
    raise SystemExit(f"Committed release build number mismatch: {release.get('build_number')} != {expected_build_number}")
if changelog.get("format") != "markdown" or not changelog.get("url"):
    raise SystemExit("Committed release did not expose a markdown changelog URL")
if full_changelog.get("format") != "markdown" or not full_changelog.get("url"):
    raise SystemExit("Committed release did not expose the full changelog URL")
PY

catalog_file="$response_dir/releases.json"
summary_phase="verifying public catalog"
curl "${curl_args[@]}" "$catalog_url" > "$catalog_file"
python3 scripts/internal/release/verify_portalsurfer_upload_catalog.py \
  --catalog-file "$catalog_file" --identity-file "$identity_file"

changelog_catalog_file="$response_dir/changelog-catalog.json"
summary_phase="verifying per-release changelog"
curl "${curl_args[@]}" \
  "$catalog_url/$BUILD_ID/changelog" > "$changelog_catalog_file"
python3 - "$changelog_catalog_file" "$BUILD_ID" "$RELEASE_LOG" <<'PY'
import json
import sys

catalog_path, expected_build, changelog_path = sys.argv[1:]
response = json.loads(open(catalog_path, encoding="utf-8").read())
if response.get("build_id") != expected_build:
    raise SystemExit(f"Fetched changelog id mismatch: {response.get('build_id')} != {expected_build}")
body = (response.get("changelog") or {}).get("body", "").strip()
expected_body = open(changelog_path, encoding="utf-8").read().strip()
if body != expected_body:
    raise SystemExit("Fetched changelog body does not match generated release log")
print(f"Uploaded Yield release log to {expected_build}")
PY

full_changelog_catalog_file="$response_dir/full-changelog-catalog.json"
summary_phase="verifying full changelog"
curl "${curl_args[@]}" \
  "$full_changelog_url" > "$full_changelog_catalog_file"
python3 - "$full_changelog_catalog_file" "$FULL_CHANGELOG_OUT" <<'PY'
import json
import sys

catalog_path, changelog_path = sys.argv[1:]
response = json.loads(open(catalog_path, encoding="utf-8").read())
body = (response.get("changelog") or {}).get("body", "").strip()
expected_body = open(changelog_path, encoding="utf-8").read().strip()
if body != expected_body:
    raise SystemExit("Fetched full changelog body does not match generated changelog")
print("Uploaded Yield full changelog")
PY
summary_phase="complete"
