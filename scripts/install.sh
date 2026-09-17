#!/usr/bin/env bash
set -euo pipefail
umask 077
# Build from a reviewed checkout. The native installer owns activation and locking.
if [[ $(uname -s) != Linux ]]; then
  echo 'Crow native installation currently supports Linux.' >&2
  exit 1
fi
command -v cargo >/dev/null || { echo 'Install the Rust toolchain, then rerun this source installer.' >&2; exit 1; }
crow_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$crow_source"
cargo build --locked --release --bin crow
export CROW_HOME=${CROW_HOME:-${CROW_INSTALL_DIR:-"$HOME/.local/share/crow"}}
export CROW_BIN_DIR=${CROW_BIN_DIR:-"$HOME/.local/bin"}
exec "${CARGO_TARGET_DIR:-target}/release/crow" install --no-setup
