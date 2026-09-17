#!/usr/bin/env bash
set -euo pipefail
crow_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$crow_source"
if [[ $(uname -s) != Linux ]]; then
  echo 'Build release binaries natively on Linux x64 or ARM64.' >&2
  exit 1
fi
case $(uname -m) in
  x86_64) crow_target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) crow_target=aarch64-unknown-linux-musl ;;
  *) echo 'Build release binaries natively on Linux x64 or ARM64.' >&2; exit 1 ;;
esac
command -v musl-gcc >/dev/null || { echo 'Install musl-tools before building portable releases.' >&2; exit 1; }
# The release executable must not inherit the build host's glibc version.
# Native musl-gcc also compiles the bundled SQLite and ring C sources.
rustup target add "$crow_target"
crow_target_key=${crow_target//-/_}
env "CARGO_TARGET_${crow_target_key^^}_LINKER=musl-gcc" "CC_${crow_target_key}=musl-gcc" \
  cargo build --locked --release --target "$crow_target" --bin crow
crow_binary="${CARGO_TARGET_DIR:-target}/$crow_target/release/crow"
scripts/check-release-compatibility.sh "$crow_binary"
# Maintenance runs on the build host; only Crow is shipped to older machines.
cargo build --locked --release --bin crow-maint
exec "${CARGO_TARGET_DIR:-target}/release/crow-maint" pack --executable "$crow_binary" "$@"
