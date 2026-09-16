#!/usr/bin/env bash
set -euo pipefail
crow_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$crow_source"
if [[ $(uname -s) != Linux ]]; then
  echo 'Build release binaries natively on Linux x64 or ARM64.' >&2
  exit 1
fi
cargo build --locked --release --bin crow --bin crow-maint
exec "${CARGO_TARGET_DIR:-target}/release/crow-maint" pack --executable "${CARGO_TARGET_DIR:-target}/release/crow" "$@"
