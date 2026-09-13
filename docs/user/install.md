# Install Crow on Linux

From a Crow checkout, run:

```sh
bash scripts/install.sh
~/.local/bin/crow setup
```

The installer uses Node 24 or newer if it is already available. Otherwise, it downloads the latest official Node 24 release for Linux x64 or arm64 into `~/.local/share/crow/node`. It retrieves the archive and SHA-256 checksums over HTTPS from nodejs.org and verifies the archive before extraction. Automatic Node installation needs `curl`, `tar` with xz support, and `sha256sum`.

Crow builds the application from its TypeScript source into a fresh staging directory using Node's built-in type stripping. It checks the emitted JavaScript syntax and runs CLI help before activating a versioned directory under `~/.local/share/crow/releases`. It does not reuse a developer's existing `dist/` directory. The `current` link selects the installed release, and `~/.local/bin/crow` starts it with the selected Node executable. There are no runtime package dependencies to install. pnpm and the TypeScript compiler are development dependencies and are not required to install or run Crow. Strict type checking runs in the development checks before release; installation checks executable output. Keep the source checkout if you want to use `crow update`.

Node may print an experimental warning for its `stripTypeScriptTypes` API during the build. The installed service runs ordinary JavaScript and does not call this API.

The installer prints a PATH command if your shell cannot find `~/.local/bin`. It does not start Crow. Run `crow setup` to configure GitHub, HTTPS, subscription authentication, and persistent startup. On a headless machine, open the printed browser URLs on your desktop. Setup checks connections and configuration without running a PR review.

Crow uses your existing official Codex executable. If it is missing, setup offers to install the latest official CLI. Installing or updating Crow does not replace an existing user-managed Codex installation.

For a custom software location, set `CROW_INSTALL_DIR` and `CROW_BIN_DIR` when running the installer. `CROW_HOME` independently controls Crow's configuration and runtime data directory. Its default is `~/.local/share/crow`. `CROW_NODE` can select an existing Node executable for installation.

## Local retention

Crow removes expired local checkouts, review workspaces, completed report files, and per-review logs for completed, superseded, or cancelled jobs. The default retention period is seven days. Paused jobs remain available for resumption while the service keeps them active. Compact review records and GitHub comments remain.

Cleanup also removes rollout files for known expired provider session IDs, including Crow's delegated reviewers, under the installation's own `codex/sessions` and `codex/archived_sessions` directories. It skips linked directories and leaves the Codex authentication file alone. It does not modify Codex's SQLite databases, whose provider-owned formats may change. A custom Codex home outside this directory needs separate retention management.
