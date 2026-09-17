#!/usr/bin/env bash
set -euo pipefail
crow_binary=${1:?Usage: check-release-compatibility.sh /path/to/crow}
command -v readelf >/dev/null || { echo 'Install binutils to verify release compatibility.' >&2; exit 1; }
crow_headers=$(LC_ALL=C readelf --wide --program-headers "$crow_binary")
crow_dynamic=$(LC_ALL=C readelf --wide --dynamic "$crow_binary")
crow_versions=$(LC_ALL=C readelf --wide --version-info "$crow_binary")
if [[ "$crow_headers" == *INTERP* || "$crow_dynamic" == *NEEDED* || "$crow_versions" == *GLIBC_* ]]; then
  echo 'Release executables must be static, without a dynamic loader, shared libraries, or versioned glibc imports.' >&2
  exit 1
fi
printf 'Verified static release executable: %s\n' "$crow_binary"
