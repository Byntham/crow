#!/usr/bin/env bash
set -euo pipefail
umask 077

# Run from an existing Crow checkout. This installs files; crow setup performs onboarding.
if [[ $(uname -s) != Linux ]]; then
  echo 'Crow native installation currently supports Linux.' >&2
  exit 1
fi
crow_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
crow_install=${CROW_INSTALL_DIR:-"$HOME/.local/share/crow"}
crow_bin=${CROW_BIN_DIR:-"$HOME/.local/bin"}
mkdir -p -- "$crow_install/releases" "$crow_bin"
crow_install=$(cd -- "$crow_install" && pwd -P)
crow_bin=$(cd -- "$crow_bin" && pwd -P)
crow_tmp=$(mktemp -d "$crow_install/.install.XXXXXXXX")
trap 'rm -rf -- "$crow_tmp"' EXIT

crow_node=${CROW_NODE:-$(command -v node || true)}
if [[ -z "$crow_node" ]] || ! "$crow_node" -e 'if(Number(process.versions.node.split(".")[0])<24)process.exit(1)' >/dev/null 2>&1; then
  crow_node="$crow_install/node/bin/node"
  if [[ ! -x "$crow_node" ]] || ! "$crow_node" -e 'if(Number(process.versions.node.split(".")[0])<24)process.exit(1)' >/dev/null 2>&1; then
    for crow_tool in curl tar sha256sum; do
      command -v "$crow_tool" >/dev/null || { echo "Install $crow_tool and rerun this installer." >&2; exit 1; }
    done
    case $(uname -m) in
      x86_64) crow_arch=x64 ;;
      aarch64|arm64) crow_arch=arm64 ;;
      *) echo 'Automatic Node installation supports Linux x64 and arm64. Install Node 24 or newer, then rerun.' >&2; exit 1 ;;
    esac
    crow_url=https://nodejs.org/dist/latest-v24.x
    echo 'Installing official Node 24 in the Crow installation directory.'
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --tlsv1.2 "$crow_url/SHASUMS256.txt" -o "$crow_tmp/SHASUMS256.txt"
    crow_archive=$(sed -nE "s/^[a-f0-9]{64}  (node-v24\.[0-9]+\.[0-9]+-linux-$crow_arch\.tar\.xz)$/\1/p" "$crow_tmp/SHASUMS256.txt")
    [[ "$crow_archive" =~ ^node-v24\.[0-9]+\.[0-9]+-linux-(x64|arm64)\.tar\.xz$ ]] || { echo 'Could not identify the official Node 24 archive.' >&2; exit 1; }
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --tlsv1.2 "$crow_url/$crow_archive" -o "$crow_tmp/$crow_archive"
    if ! (cd "$crow_tmp" && sed -n "/  $crow_archive\$/p" SHASUMS256.txt | sha256sum --check --status); then
      echo 'Node archive checksum verification failed. No release was activated; rerun the installer to download it again.' >&2
      exit 1
    fi
    mkdir "$crow_tmp/node"
    tar -xJf "$crow_tmp/$crow_archive" --strip-components=1 -C "$crow_tmp/node"
    "$crow_tmp/node/bin/node" -e 'if(Number(process.versions.node.split(".")[0])!==24)process.exit(1)'
    if [[ -e "$crow_install/node" || -L "$crow_install/node" ]]; then
      echo "An unusable Crow Node installation exists at $crow_install/node; move it aside and rerun." >&2
      exit 1
    fi
    mv -- "$crow_tmp/node" "$crow_install/node"
  fi
fi
crow_node=$("$crow_node" -p 'process.execPath')
# Compile with Node's built-in TypeScript stripping. Developers verify strict types
# before release; installing from a checkout needs neither pnpm nor TypeScript.
"$crow_node" "$crow_source/scripts/build.mjs" --out "$crow_tmp/release"
for crow_item in docs package.json README.md LICENSE CONTEXT.md; do
  if [[ -e "$crow_source/$crow_item" ]]; then cp -R -- "$crow_source/$crow_item" "$crow_tmp/release/"; fi
done
[[ -f "$crow_tmp/release/bin/crow.mjs" ]] || { echo 'Run this installer from a complete Crow checkout.' >&2; exit 1; }
"$crow_node" --input-type=module - "$crow_tmp/release/package.json" <<'NODE'
import {readFileSync, writeFileSync} from 'node:fs';
const file=process.argv[2], pkg=JSON.parse(readFileSync(file,'utf8'));
pkg.bin={crow:'bin/crow.mjs'};
delete pkg.scripts;
delete pkg.devDependencies;
writeFileSync(file,JSON.stringify(pkg,null,2)+'\n');
NODE
"$crow_node" "$crow_tmp/release/bin/crow.mjs" help >/dev/null
crow_release=$("$crow_node" --input-type=module - "$crow_tmp/release" <<'NODE'
import { readFileSync, readdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, relative } from 'node:path';
const root=process.argv[2], hash=createHash('sha256');
function walk(dir) { for(const entry of readdirSync(dir,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))) { const file=join(dir,entry.name); if(entry.isDirectory())walk(file);else if(entry.isFile()){hash.update(relative(root,file));hash.update('\0');hash.update(readFileSync(file));}else throw new Error('Crow release contains an unsupported linked file'); } }
walk(root);
const version=JSON.parse(readFileSync(join(root,'package.json'),'utf8')).version;
if(!/^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?$/.test(version))throw new Error('Invalid package version');
console.log(`${version}-${hash.digest('hex').slice(0,16)}`);
NODE
)
"$crow_node" --input-type=module - "$crow_tmp/release/install.json" "$crow_source" "$crow_install" "$crow_bin" <<'NODE'
import {writeFileSync} from 'node:fs';
import {spawnSync} from 'node:child_process';
const revision=spawnSync('git',['rev-parse','HEAD'],{cwd:process.argv[3],encoding:'utf8'});
writeFileSync(process.argv[2],JSON.stringify({source:process.argv[3],installation:process.argv[4],bin:process.argv[5],revision:revision.status===0?revision.stdout.trim():null,installedAt:new Date().toISOString()},null,2)+'\n',{mode:0o600});
NODE
if [[ ! -e "$crow_install/releases/$crow_release" ]]; then
  mv -- "$crow_tmp/release" "$crow_install/releases/$crow_release"
else
  # Identical runtime files may come from a newer source revision. Replace only
  # the installation metadata atomically so later updates see that revision.
  mv -Tf -- "$crow_tmp/release/install.json" "$crow_install/releases/$crow_release/install.json"
fi
ln -s -- "releases/$crow_release" "$crow_tmp/current"
if [[ -e "$crow_install/current" && ! -L "$crow_install/current" ]]; then echo "Refusing to replace directory $crow_install/current" >&2; exit 1; fi
mv -Tf -- "$crow_tmp/current" "$crow_install/current"
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf 'export PATH=%q:"$PATH"\n' "$(dirname -- "$crow_node")"
  printf 'exec %q %q "$@"\n' "$crow_node" "$crow_install/current/bin/crow.mjs"
} > "$crow_tmp/crow"
chmod 755 "$crow_tmp/crow"
mv -f -- "$crow_tmp/crow" "$crow_bin/crow"
echo "Installed Crow $crow_release."
printf 'Run %q setup to configure this installation.\n' "$crow_bin/crow"
case ":$PATH:" in *":$crow_bin:"*) ;; *) printf 'Add the CLI to PATH: export PATH=%q:"$PATH"\n' "$crow_bin" ;; esac
echo 'This installer does not start services, publish HTTPS routes, or modify your Codex installation.'
