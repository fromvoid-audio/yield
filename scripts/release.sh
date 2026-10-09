#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/release.sh build --sha <40-character-commit-sha> --channel <test|beta|release>

Build locally through scripts/yield-local-ci. Test apps use an isolated
namespace; beta and release apps use the normal saved Yield state.
Building does not launch the app or reset state.
USAGE
}

die() {
  echo "release: $*" >&2
  exit 2
}

if (( $# == 0 )); then
  usage
  exit 0
fi

command="$1"
shift
case "$command" in
  -h|--help)
    usage
    exit 0
    ;;
  build)
    for argument in "$@"; do
      [[ "$argument" != "--dispatch" ]] || die "--dispatch is obsolete; use scripts/yield-local-ci build --sha <40-character-commit-sha> --channel <test|beta|release>"
    done
    script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
    exec "$script_dir/yield-local-ci" build "$@"
    ;;
  prepare|rc|stable|--dispatch)
    die "'$command' is obsolete; use scripts/yield-local-ci build --sha <40-character-commit-sha> --channel <test|beta|release>"
    ;;
  *)
    die "unknown command '$command'; use scripts/yield-local-ci build --sha <40-character-commit-sha> --channel <test|beta|release>"
    ;;
esac
