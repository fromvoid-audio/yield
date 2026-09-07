#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec cargo run --locked --manifest-path "$repo_dir/apps/wavecrate-gpui/Cargo.toml" -- "$@"
