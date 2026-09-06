#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="${VELNOR_WORKFLOW_SOURCE_DIR:-$repo_root/../velnor}"
manifest="$source_root/Cargo.toml"

if test ! -f "$manifest"; then
  echo "Velnor source is required at $source_root; set VELNOR_WORKFLOW_SOURCE_DIR" >&2
  exit 1
fi

exec cargo run --manifest-path "$manifest" --locked -p velnor-workflow -- "$@"
