# Install Crow on Linux

Crow's recommended distribution is a Linux executable containing the Node 24 LTS runtime. Users do not need Node, pnpm, a TypeScript compiler, or the source repository.

Download `v0.2.0` from [GitHub Releases](https://github.com/Byntham/Crow/releases/tag/v0.2.0), use the source installation below, or build a binary with `pnpm build:binary` in a development checkout.

## Download and setup

The release archives are `crow-v0.2.0-linux-x64.tar.gz` and `crow-v0.2.0-linux-arm64.tar.gz`. Gibo uses x64. Choose arm64 for an ARM64 host. These binaries use glibc and the system C++ runtime; Alpine and other musl distributions need a different distribution and are not supported by these artifacts.

While the repository is private, authenticate GitHub CLI with an account that has access, then download the selected archive and checksums:

```sh
gh auth login
gh release download v0.2.0 --repo Byntham/Crow \
  --pattern crow-v0.2.0-linux-x64.tar.gz --pattern SHA256SUMS \
  --dir crow-download
cd crow-download
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf crow-v0.2.0-linux-x64.tar.gz
./crow setup --role both --ingress funnel
```

Stop if checksum verification fails. `--ignore-missing` allows the checksum file to list the other architecture, which you did not download. For arm64, replace both archive names with `crow-v0.2.0-linux-arm64.tar.gz`. You can also download the files through the GitHub release page on your desktop and transfer them to the host before verification.

Run setup as your normal user. It installs the binary under the directory chosen by `CROW_HOME`, with `current/crow` selecting the active release and a stable command in `~/.local/bin/crow`. The default installation and state directory is `~/.local/share/crow`. Add `~/.local/bin` to your `PATH` if requested. After setup completes, the download directory is no longer needed.

Setup guides GitHub, public HTTPS, subscription authentication, and persistent systemd startup. It offers installation of missing supported dependencies, including Git, GitHub CLI, Codex, and ingress tools. On a headless machine, open the printed browser URLs on your desktop. Setup checks connections and configuration without running a PR review.

Crow uses your existing official Codex executable. If it is missing, setup offers to install the latest official CLI. Installing or updating Crow does not replace an existing user-managed Codex installation.

See the [quickstart](quickstart.md) for the setup prompts and [operations](operations.md#lifecycle-and-updates) for binary updates.

## Install from source

Source installation remains available. From a Crow checkout, run:

```sh
bash scripts/install.sh
~/.local/bin/crow setup
```

The installer uses Node 24 or newer if it is already available. Otherwise, it downloads the latest official Node 24 release for Linux x64 or arm64 into `~/.local/share/crow/node`. It retrieves the archive and SHA-256 checksums over HTTPS from nodejs.org and verifies the archive before extraction. Automatic Node installation needs `curl`, `tar` with xz support, and `sha256sum`.

Crow builds the application from its TypeScript source into a fresh staging directory using Node's built-in type stripping. It checks the emitted JavaScript syntax and runs CLI help before activating a versioned directory under `~/.local/share/crow/releases`. It does not reuse a developer's existing `dist/` directory. The `current` link selects the installed release, and `~/.local/bin/crow` starts it with the selected Node executable. There are no runtime package dependencies to install. pnpm and the TypeScript compiler are development dependencies and are not required to install or run Crow. Strict type checking runs in the development checks before release; installation checks executable output. Keep the source checkout if you want to use `crow update`.

Node may print an experimental warning for its `stripTypeScriptTypes` API during the build. The installed service runs ordinary JavaScript and does not call this API.

The installer prints a PATH command if your shell cannot find `~/.local/bin`. It does not start Crow. Run `crow setup` to configure GitHub, HTTPS, subscription authentication, and persistent startup. On a headless machine, open the printed browser URLs on your desktop. Setup checks connections and configuration without running a PR review.

For a custom software location, set `CROW_INSTALL_DIR` and `CROW_BIN_DIR` when running the installer. `CROW_HOME` independently controls Crow's configuration and runtime data directory. Its default is `~/.local/share/crow`. `CROW_NODE` can select an existing Node executable for installation.

## Local retention

Crow removes expired local checkouts, review workspaces, completed report files, and per-review logs for completed, superseded, or cancelled jobs. The default retention period is seven days. Paused jobs remain available for resumption while the service keeps them active. Compact review records and GitHub comments remain.

Cleanup also removes rollout files for known expired provider session IDs, including Crow's delegated reviewers, under the installation's own `codex/sessions` and `codex/archived_sessions` directories. It skips linked directories and leaves the Codex authentication file alone. It does not modify Codex's SQLite databases, whose provider-owned formats may change. A custom Codex home outside this directory needs separate retention management.
