# Install Crow on Linux

Crow's hosted installer downloads a native Linux executable containing Node 24 LTS. Users do not need the source repository, Node, pnpm, or a TypeScript compiler. The installer and binaries will be public even while the source repository is private.

**The hosted installer is prepared locally but has not been deployed.** The commands below become available after the first approved publication. Until then, use the source installation or build a binary with `pnpm build:binary`.

## Hosted installation

From an SSH terminal on the machine that will run Crow, use:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh
```

Run this as your normal user, without sudo. The installer detects Linux x64 or ARM64, checks prerequisites, downloads a versioned archive from `downloads.birdapp.dev`, and verifies its checksum before extraction. It then installs the binary and offers to start setup. Ubuntu with systemd is the supported guided setup platform. These binaries require glibc and the system C++ runtime; Alpine and other musl distributions are unsupported.

The bootstrap needs common Linux tools including `curl`, `tar`, and `sha256sum`. It explains missing prerequisites before installation. Crow setup offers installation of missing supported dependencies such as Git, GitHub CLI, Codex, and the chosen ingress client. GitHub login happens during setup to connect your repositories, not to download Crow.

On a headless machine, open the printed browser URLs on your desktop. The installer reads setup prompts from your terminal even when invoked through the pipe above. Without a terminal, it installs only and prints the command to start setup later. Setup checks connections and configuration without running a PR review.

To install without entering setup, or to select an explicit version:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh -s -- --no-setup
curl -fsSL https://birdapp.dev/install.sh | sh -s -- --version X.Y.Z --no-setup
```

Replace `X.Y.Z` with a published version. The installer cannot downgrade or replace a different existing Crow executable. Use the installed `crow update` command for upgrades, or `crow setup` to continue onboarding. Repeating installation of the identical binary preserves configuration and active work.

The default installation and state directory is `~/.local/share/crow`. `current/crow` selects the installed release and `~/.local/bin/crow` is the stable command. Set `CROW_HOME` for a custom state directory and use that same value for later commands. If `~/.local/bin` is missing from your current shell's PATH, the installer prints the command to add it and still launches setup using the full executable path. Add that directory to your shell startup file if it is not already configured for future sessions.

If setup is interrupted, the permanent command remains installed. Continue with `crow setup`, or the exact command printed by the installer. Crow reuses your existing official Codex executable. If it is missing, setup can install the latest official standalone package without npm. Installing Crow does not replace a user-managed Codex installation.

## Manual binary installation

For environments where piping an installer is inconvenient, download the versioned archive and `SHA256SUMS` from the installation page, then verify and extract it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf crow-vX.Y.Z-linux-x64.tar.gz
./crow install
```

Stop if checksum verification fails. Use the actual downloaded filename and substitute `arm64` when appropriate. `--ignore-missing` permits the checksum file to include the other architecture. `./crow install --no-setup` installs without starting onboarding. You can transfer these files from your desktop to the host before verification. There is no published release to download until the first approved publication.

See the [quickstart](quickstart.md) for setup prompts and [operations](operations.md#lifecycle-and-updates) for updates.

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
