#!/bin/sh
# Download and install a published Crow binary. No repository checkout is needed.

crow_fail() {
  printf 'Crow installer: %s\n' "$*" >&2
  exit 1
}

crow_fetch() {
  curl --disable --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' \
    --connect-timeout 15 --max-time 300 --retry 2 --retry-delay 2 \
    --output "$2" "$1" || crow_fail "Download failed: $1. Rerun the installer to try again."
}

crow_valid_version() {
  printf '%s\n' "$1" | awk '
    NR != 1 || length($0) > 64 || $0 !~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ { invalid = 1 }
    END { exit (NR != 1 || invalid) }
  '
}

crow_main() {
  set -eu
  umask 077
  LC_ALL=C
  export LC_ALL
  # User tar defaults must not change validation or extraction behavior.
  unset TAR_OPTIONS
  crow_version=
  crow_no_setup=false
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version)
        [ "$#" -ge 2 ] || crow_fail '--version requires X.Y.Z.'
        crow_version=$2
        [ -n "$crow_version" ] || crow_fail '--version requires X.Y.Z.'
        shift 2
        ;;
      --no-setup) crow_no_setup=true; shift ;;
      --help|-h)
        printf '%s\n' 'Install Crow: sh install.sh [--version X.Y.Z] [--no-setup]' \
          'Supports Linux x64 and ARM64 with glibc. Installs under your user account.' \
          'Without a terminal, installs only; run crow setup afterward.'
        return 0
        ;;
      *) crow_fail "Unknown option: $1. Use --help for usage." ;;
    esac
  done
  for crow_command in uname getconf curl tar sha256sum mktemp awk chmod rm; do
    command -v "$crow_command" >/dev/null 2>&1 || crow_fail "Missing $crow_command. Install it with your system package manager and retry."
  done
  [ "$(uname -s)" = Linux ] || crow_fail 'Crow currently supports Linux only.'
  case "$(uname -m)" in
    x86_64|amd64) crow_arch=x64 ;;
    aarch64|arm64) crow_arch=arm64 ;;
    *) crow_fail 'Crow currently supports x64 and ARM64 machines only.' ;;
  esac
  case "$(getconf GNU_LIBC_VERSION 2>/dev/null || true)" in
    'glibc '*) ;;
    *) crow_fail 'Crow requires glibc Linux, such as Ubuntu or Debian. Alpine/musl is unsupported.' ;;
  esac
  if [ -n "$crow_version" ]; then
    crow_valid_version "$crow_version" || crow_fail 'Invalid version. Use a stable version such as 0.2.0.'
  fi
  crow_temp=$(mktemp -d "${TMPDIR:-/tmp}/crow-install.XXXXXXXXXX") || crow_fail 'Could not create a temporary directory.'
  trap 'rm -rf -- "$crow_temp"' 0
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  crow_host=https://downloads.birdapp.dev
  if [ -z "$crow_version" ]; then
    crow_fetch "$crow_host/latest.txt" "$crow_temp/latest.txt"
    crow_version=$(awk 'NR == 1 { print; next } { exit 1 }' "$crow_temp/latest.txt") || crow_fail 'Invalid latest-version response.'
    crow_valid_version "$crow_version" || crow_fail 'Invalid latest-version response.'
  fi
  crow_archive="crow-v$crow_version-linux-$crow_arch.tar.gz"
  crow_release="$crow_host/releases/v$crow_version"
  printf 'Downloading Crow %s for Linux %s...\n' "$crow_version" "$crow_arch"
  crow_fetch "$crow_release/SHA256SUMS" "$crow_temp/SHA256SUMS"
  crow_fetch "$crow_release/$crow_archive" "$crow_temp/$crow_archive"
  crow_digest=$(awk -v archive="$crow_archive" '
    $2 == archive {
      if (NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-fA-F]/) invalid = 1
      count++; digest = $1
    }
    END { if (count != 1 || invalid) exit 1; print digest }
  ' "$crow_temp/SHA256SUMS") || crow_fail 'The release checksum is missing, duplicated, or invalid.'
  printf '%s  %s\n' "$crow_digest" "$crow_archive" > "$crow_temp/selected.sha256"
  (cd "$crow_temp" && sha256sum --check --status selected.sha256) || crow_fail 'Checksum verification failed. Nothing was installed.'
  tar -tzf "$crow_temp/$crow_archive" > "$crow_temp/entries" || crow_fail 'Could not read the release archive.'
  awk '
    $0 == "crow" || $0 == "./crow" { binary++; next }
    $0 == "THIRD_PARTY_NOTICES" || $0 == "./THIRD_PARTY_NOTICES" { notices++; next }
    { invalid = 1 }
    END { exit (invalid || binary != 1 || notices != 1) }
  ' "$crow_temp/entries" || crow_fail 'The release archive contains unexpected files.'
  tar -tvzf "$crow_temp/$crow_archive" > "$crow_temp/details" || crow_fail 'Could not inspect the release archive.'
  awk 'substr($0, 1, 1) != "-" { invalid = 1 } END { exit (invalid || NR != 2) }' \
    "$crow_temp/details" || crow_fail 'The release archive must contain regular files only.'
  tar -xzf "$crow_temp/$crow_archive" -C "$crow_temp" --no-same-owner --no-same-permissions || crow_fail 'Could not extract the release archive.'
  chmod 700 "$crow_temp/crow"
  crow_actual=$("$crow_temp/crow" --version) || crow_fail 'Crow could not run. Check that this machine supports the published Linux binary.'
  [ "$crow_actual" = "$crow_version" ] || crow_fail 'The downloaded executable reports an unexpected version.'
  if [ "$crow_no_setup" = true ]; then
    "$crow_temp/crow" install --no-setup </dev/null
  elif ( : </dev/tty ) 2>/dev/null; then
    "$crow_temp/crow" install </dev/tty
  else
    printf '%s\n' 'No terminal detected. Installing only; run crow setup afterward.'
    "$crow_temp/crow" install --no-setup </dev/null
  fi
}

# Keep this invocation last so a truncated download cannot begin installation.
crow_main "$@"
