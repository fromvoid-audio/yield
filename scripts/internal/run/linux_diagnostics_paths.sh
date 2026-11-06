#!/usr/bin/env bash

# Resolve the installed Linux profile without creating or reading app state.
# Keep in step with yield-library's app_dirs/resolution.rs and the sandbox runner.
resolve_linux_diagnostics_paths() {
  local caller_dir="$1" sandbox_base="$2" sandbox="$3"
  local raw_profile="${YIELD_CONFIG_PROFILE:-}" lower_profile
  local LC_ALL=C

  used_sandbox_config_home=false
  if [[ -n "${YIELD_CONFIG_HOME+x}" ]]; then
    config_base_dir="$YIELD_CONFIG_HOME"
  elif (( sandbox )); then
    config_base_dir="$sandbox_base"
    used_sandbox_config_home=true
  elif [[ "${XDG_CONFIG_HOME:-}" == /* ]]; then
    config_base_dir="$XDG_CONFIG_HOME"
  elif [[ -n "${HOME:-}" ]]; then
    config_base_dir="${HOME}/.config"
  else
    printf '%s\n' "Unable to resolve Yield config base. Set HOME or YIELD_CONFIG_HOME." >&2
    return 1
  fi
  case "$config_base_dir" in
    /*) ;;
    "") config_base_dir="$caller_dir" ;;
    *) config_base_dir="${caller_dir}/${config_base_dir}" ;;
  esac

  # Trim profile environment whitespace before resolving the runtime aliases.
  raw_profile="${raw_profile#"${raw_profile%%[![:space:]]*}"}"
  raw_profile="${raw_profile%"${raw_profile##*[![:space:]]}"}"
  lower_profile="$(printf '%s' "$raw_profile" | LC_ALL=C tr '[:upper:]' '[:lower:]')"
  if (( sandbox )); then
    resolved_profile=sandbox
  else
    case "$lower_profile" in
      ""|live) resolved_profile=live ;;
      sandbox) resolved_profile=sandbox ;;
      automated) resolved_profile=automated-tests ;;
      *) resolved_profile="$raw_profile" ;;
    esac
  fi
  if [[ ! "$resolved_profile" =~ ^[a-zA-Z0-9_.-]+$ || "$resolved_profile" == . || "$resolved_profile" == .. ]]; then
    printf '%s\n' "Invalid Yield diagnostics profile name." >&2
    return 2
  fi

  app_root="${config_base_dir%/}/.yield"
  if [[ "$resolved_profile" != live ]]; then
    app_root="${app_root}/profiles/${resolved_profile}"
  fi
  logs_dir="${app_root}/logs"
  config_path="${app_root}/config.toml"
}
