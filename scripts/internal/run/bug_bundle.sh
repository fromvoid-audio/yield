#!/usr/bin/env bash

# Creates a small diagnostic bundle to attach to bug reports.
#
# Linux bundles contain only allowlisted status fields. Other Bash platforms
# retain the existing bundle contents:
# - latest logs (default: 5)
# - `config.toml` (if present)
# - tool/runtime versions (`rustc`, `cargo`, `git`)
#
# Note: logs and config may contain local paths. Review before sharing.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
invocation_dir="$PWD"
os_name="$(uname -s | tr '[:upper:]' '[:lower:]')"
cd "$ROOT_DIR"

MAX_LOGS=5
OUT_DIR="dist/bug_bundles"
USE_SANDBOX=0
if [[ "$os_name" == linux ]]; then
  OUT_DIR="yield-support"
fi

usage() {
  local entrypoint="${YIELD_RUN_ENTRYPOINT:-scripts/run.sh}"
  if [[ "$os_name" == linux ]]; then
    cat <<EOF
Usage: ${entrypoint} bug-bundle [--out-dir <dir>] [--logs <1-10>] [--sandbox]

Creates a private support archive under <dir> (default: ./yield-support).
Reports log count up to the given limit (default: 5), configuration presence,
and basic host facts. View the newest log locally with the logs command.
EOF
    return
  fi
  cat <<EOF
Usage: ${entrypoint} bug-bundle [--out-dir <dir>] [--logs <n>] [--sandbox]

Creates an archive under <out-dir> containing:
- the newest N log files (default: 5)
- config.toml (if present)
- version/system info

Sandbox behavior:
- If YIELD_CONFIG_HOME is set, it is always used.
- Otherwise, if <repo>/.sandbox/yield exists, this script prefers it.
- Pass --sandbox to force using <repo>/.sandbox/yield.
EOF
}

while (( $# > 0 )); do
  case "$1" in
    --out-dir)
      if (( $# < 2 )) || [[ -z "$2" ]]; then usage >&2; exit 2; fi
      OUT_DIR="${2:-}"; shift 2 ;;
    --logs)
      if (( $# < 2 )) || [[ -z "$2" ]]; then usage >&2; exit 2; fi
      MAX_LOGS="${2:-}"; shift 2 ;;
    --sandbox)
      USE_SANDBOX=1; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "[bug_bundle] Unknown argument: $1" >&2
      usage >&2
      exit 2 ;;
  esac
done

sandbox_config_home="${ROOT_DIR}/.sandbox/yield"

default_config_base_dir() {
  if [[ -n "${YIELD_CONFIG_HOME:-}" ]]; then
    printf "%s" "$YIELD_CONFIG_HOME"
    return 0
  fi
  if (( USE_SANDBOX == 1 )) || [[ -d "$sandbox_config_home" ]]; then
    printf "%s" "$sandbox_config_home"
    return 0
  fi
  case "$os_name" in
    darwin)
      printf "%s" "${HOME}/Library/Application Support"
      ;;
    *)
      if [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
        printf "%s" "$XDG_CONFIG_HOME"
      else
        printf "%s" "${HOME}/.config"
      fi
      ;;
  esac
}

current_persistence_profile() {
  if [[ -n "${YIELD_CONFIG_PROFILE:-}" ]]; then
    printf "%s" "${YIELD_CONFIG_PROFILE}"
    return 0
  fi
  if (( USE_SANDBOX == 1 )); then
    printf "%s" "sandbox"
    return 0
  fi
  printf "%s" "live"
}

extract_app_data_dir_from_config() {
  local config_path="$1"
  [[ -f "$config_path" ]] || return 1
  local line
  line="$(rg -m 1 -N '^\\s*app_data_dir\\s*=\\s*' "$config_path" 2>/dev/null || true)"
  [[ -n "$line" ]] || return 1
  local value="${line#*=}"
  value="$(printf "%s" "$value" | sed -E 's/^\\s+//; s/\\s+$//; s/\\s+#.*$//')"
  value="$(printf "%s" "$value" | sed -E 's/^\"(.*)\"$/\\1/; s/^\\x27(.*)\\x27$/\\1/')"
  [[ -n "$value" ]] || return 1
  printf "%s" "$value"
  return 0
}

resolve_app_root_dir() {
  local profile
  profile="$(current_persistence_profile)"
  local default_root="${config_base_dir}/.yield"
  if [[ "$profile" != "live" ]]; then
    default_root="${default_root}/profiles/${profile}"
  fi

  local config_path="${default_root}/config.toml"
  local override_root=""
  if override_root="$(extract_app_data_dir_from_config "$config_path")"; then
    if [[ "$override_root" = /* ]]; then
      printf "%s" "$override_root"
      return 0
    fi
  fi
  printf "%s" "$default_root"
}

if [[ "$os_name" == linux ]]; then
  source "$ROOT_DIR/scripts/internal/run/linux_diagnostics_paths.sh"
  resolve_linux_diagnostics_paths "$invocation_dir" "$sandbox_config_home" "$USE_SANDBOX"
else
  config_base_dir="$(default_config_base_dir)"
  used_sandbox_config_home="false"
  if [[ -z "${YIELD_CONFIG_HOME:-}" ]] && [[ "$config_base_dir" == "$sandbox_config_home" ]]; then
    used_sandbox_config_home="true"
  fi
  app_root="$(resolve_app_root_dir)"
  logs_dir="${app_root}/logs"
  config_path="${app_root}/config.toml"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

if [[ "$os_name" == linux ]]; then
  if [[ ! "$MAX_LOGS" =~ ^([1-9]|10)$ ]]; then
    echo "[bug_bundle][error] --logs expects an integer from 1 to 10." >&2
    exit 2
  fi
  if [[ "$OUT_DIR" != /* ]]; then
    OUT_DIR="${invocation_dir}/${OUT_DIR}"
  fi
  umask 077
  mkdir -p "$OUT_DIR"
  bundle_dir="$(mktemp -d "${OUT_DIR}/yield-bug-bundle-${timestamp}-XXXXXXXX")"
  trap 'rm -rf -- "$bundle_dir"' EXIT

  architecture="$(uname -m)"
  case "$architecture" in
    x86_64|aarch64) ;;
    *) architecture=unknown ;;
  esac
  config_present=false
  if [[ -f "$config_path" ]]; then config_present=true; fi
  log_count=0
  for log_file in "$logs_dir"/yield_*.log; do
    [[ -f "$log_file" && ! -L "$log_file" ]] || continue
    name="${log_file##*/}"
    [[ "$name" =~ ^yield_[[:xdigit:]]{16}_[[:xdigit:]]{8}_[[:xdigit:]]{8}_[[:xdigit:]]{8}\.log$ ]] || continue
    log_count=$((log_count + 1))
    (( log_count >= MAX_LOGS )) && break
  done
  {
    printf 'timestamp_utc=%s\n' "$timestamp"
    printf 'os=linux\narchitecture=%s\n' "$architecture"
    printf 'config_present=%s\n' "$config_present"
    printf 'log_count_capped=%s\nlog_count_limit=%s\n' "$log_count" "$MAX_LOGS"
  } > "${bundle_dir}/info.txt"

  if command -v zip >/dev/null 2>&1; then
    archive="${bundle_dir}/support.zip"
    (unset ZIPOPT; zip -qjX "$archive" "${bundle_dir}/info.txt")
  else
    archive="${bundle_dir}/support.tar.gz"
    (unset TAR_OPTIONS GZIP; tar --format=ustar --owner=0 --group=0 --numeric-owner \
      -czf "$archive" -C "$bundle_dir" info.txt)
  fi
  rm -- "${bundle_dir}/info.txt"
  trap - EXIT
  echo "[bug_bundle] wrote ${archive}"
  exit 0
fi

bundle_root="$(mktemp -d)"
trap 'rm -rf "$bundle_root"' EXIT

bundle_dir="${bundle_root}/yield-bug-bundle-${timestamp}"
mkdir -p "$bundle_dir"

mkdir -p "${bundle_dir}/meta"
{
  echo "timestamp_utc=${timestamp}"
  echo "repo_root=${ROOT_DIR}"
  echo "config_base_dir=${config_base_dir}"
  echo "persistence_profile=$(current_persistence_profile)"
  echo "preferred_sandbox_config_home=${sandbox_config_home}"
  echo "used_sandbox_config_home=${used_sandbox_config_home}"
  echo "app_root=${app_root}"
  echo "logs_dir=${logs_dir}"
  echo "config_path=${config_path}"
  echo
  echo "rustc_version=$(rustc --version 2>/dev/null || echo 'n/a')"
  echo "cargo_version=$(cargo --version 2>/dev/null || echo 'n/a')"
  echo "git_version=$(git --version 2>/dev/null || echo 'n/a')"
  echo "uname=$(uname -a 2>/dev/null || echo 'n/a')"
} > "${bundle_dir}/meta/info.txt"

if [[ -f "$config_path" ]]; then
  mkdir -p "${bundle_dir}/config"
  cp "$config_path" "${bundle_dir}/config/config.toml"
fi

if [[ -d "$logs_dir" ]]; then
  mkdir -p "${bundle_dir}/logs"
  logs=()
  while IFS= read -r log_file; do
    name="$(basename "$log_file")"
    if [[ "$name" =~ ^yield_[[:xdigit:]]{16}_[[:xdigit:]]{8}_[[:xdigit:]]{8}_[[:xdigit:]]{8}\.log$ ]] && [[ -f "$log_file" ]] && [[ ! -L "$log_file" ]]; then
      logs+=("$log_file")
      (( ${#logs[@]} >= MAX_LOGS )) && break
    fi
  done < <(ls -1t "$logs_dir"/yield_*.log 2>/dev/null || true)
  if (( ${#logs[@]} == 0 )); then
    while IFS= read -r log_file; do
      [[ -f "$log_file" && ! -L "$log_file" ]] || continue
      logs+=("$log_file")
      (( ${#logs[@]} >= MAX_LOGS )) && break
    done < <(ls -1t "$logs_dir"/*.log 2>/dev/null || true)
  fi
  for log_file in "${logs[@]}"; do
    cp "$log_file" "${bundle_dir}/logs/$(basename "$log_file")"
  done
fi

mkdir -p "$OUT_DIR"
archive_base="${OUT_DIR}/yield-bug-bundle-${timestamp}"

if command -v zip >/dev/null 2>&1; then
  (cd "$bundle_root" && zip -r "${archive_base}.zip" "$(basename "$bundle_dir")" >/dev/null)
  echo "[bug_bundle] wrote ${archive_base}.zip"
elif command -v tar >/dev/null 2>&1; then
  (cd "$bundle_root" && tar -czf "${archive_base}.tar.gz" "$(basename "$bundle_dir")")
  echo "[bug_bundle] wrote ${archive_base}.tar.gz"
else
  echo "[bug_bundle][error] No archive tool found (zip or tar required)." >&2
  exit 1
fi

echo "[bug_bundle] NOTE: logs/config may contain local paths; review before sharing."
