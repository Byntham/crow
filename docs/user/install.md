# Install Crow on Linux

Crow's hosted installer downloads a native Linux executable compiled from Rust. Users do not need the source repository or a language runtime. The installer and binaries will be public even while the source repository is private.

**The hosted installer is prepared locally but has not been deployed.** The commands below become available after the first approved publication. Until then, use the source installation or build a binary with `bash scripts/build-release.sh`.

## Hosted installation

From an SSH terminal on the machine that will run Crow, use:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh
```

Run this as your normal user, without sudo. The installer detects Linux x64 or ARM64, checks prerequisites, downloads a versioned archive from `downloads.birdapp.dev`, and verifies its checksum before extraction. It then installs the binary and offers to start setup. Ubuntu with systemd is the supported guided setup platform. These binaries require glibc; Alpine and other musl distributions are unsupported.

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

Install the stable Rust toolchain, a C compiler, and Git before running the source installer. It runs `cargo build --locked --release --bin crow` and installs the resulting executable through the same installer used by downloaded binaries. SQLite is compiled into Crow; no Node or JavaScript build tools are needed.

The installer activates a versioned directory under `~/.local/share/crow/releases`. The `current` link selects the release, and `~/.local/bin/crow` is the stable command. It does not start services. Run `crow setup` to configure GitHub, HTTPS, subscription authentication, and persistent startup.

Set `CROW_HOME` for a custom installation and data directory and `CROW_BIN_DIR` for the permanent command directory. Keep the same `CROW_HOME` for later commands. `crow update` installs published native releases for both source-built and downloaded installations. It does not modify your source checkout. To test local source changes, build and run the executable from the checkout with a separate `CROW_HOME`.

## Migrating an existing installation

The Rust release is version 0.3.0. Its configuration, SQLite records, encrypted backups, and worker protocol retain their existing formats. Save an encrypted backup before upgrading. A published 0.2.0 binary installation can use `crow update` once 0.3.0 is published.

Legacy source installations use a Node launcher and a different release layout. Stop the old service, back up its state, and preserve the old launcher and `current` link before installing the Rust executable. The native installer refuses to overwrite an unrelated or different existing launcher. Keep `config.json`, `service.sqlite`, review directories, and the installation's `codex` directory in place. Run the new executable's `install --no-setup` with the same `CROW_HOME`, then `crow setup` to regenerate the systemd unit and validate connections. Do not delete saved provider sessions or reuse the personal Codex home.

Keep the previous executable and backup until the new service passes `crow doctor --runtime`. Local tests establish data-format compatibility; validate real account access on the enrolled installation before removing the old release.

## Local retention

Crow removes expired local checkouts, review workspaces, completed report files, and per-review logs for completed, superseded, or cancelled jobs. The default retention period is seven days. Paused jobs remain available for resumption while the service keeps them active. Compact review records and GitHub comments remain.

Cleanup also removes rollout files for known expired provider session IDs, including Crow's delegated reviewers, under the installation's own `codex/sessions` and `codex/archived_sessions` directories. It skips linked directories and leaves the Codex authentication file alone. It does not modify Codex's SQLite databases, whose provider-owned formats may change. A custom Codex home outside this directory needs separate retention management.
