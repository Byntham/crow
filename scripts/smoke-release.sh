#!/usr/bin/env bash
set -euo pipefail
crow_binary=${1:?Usage: smoke-release.sh /absolute/path/to/crow}
[[ "$crow_binary" = /* ]] || { echo 'Pass an absolute executable path.' >&2; exit 1; }
crow_tmp=$(mktemp -d)
trap 'rm -rf -- "$crow_tmp"' EXIT
crow_run() {
  env -i HOME="$crow_tmp" CROW_HOME="$crow_tmp/data" CROW_BIN_DIR="$crow_tmp/bin" PATH=/nonexistent "$@"
}
crow_run "$crow_binary" help >/dev/null
crow_version=$(crow_run "$crow_binary" --version)
[[ "$crow_version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
crow_run "$crow_binary" install --no-setup
[[ $(crow_run "$crow_tmp/bin/crow" --version) = "$crow_version" ]]
printf 'saved onboarding\n' > "$crow_tmp/data/config.json"
crow_run "$crow_binary" install --no-setup
[[ $(cat "$crow_tmp/data/config.json") = 'saved onboarding' ]]
printf '{"dir":"%s","head":"%040d","base":"%040d"}\n' "$crow_tmp" 0 1 > "$crow_tmp/source.json"
printf '{"jsonrpc":"2.0","id":1,"method":"initialize"}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' | crow_run "$crow_binary" _inspection-mcp "$crow_tmp/source.json" > "$crow_tmp/mcp.jsonl"
jq -se 'length == 2 and .[0].result.serverInfo.name == "crow-inspection" and ([.[1].result.tools[].name] | sort) == ["diff", "list_files", "read_file", "search"]' "$crow_tmp/mcp.jsonl" >/dev/null
