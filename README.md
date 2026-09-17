# Crow

Crow reviews GitHub pull requests on your own Linux machine, using your Codex subscription. It responds on GitHub with advisory inline findings and a summary intended for coding agents.

Each operator runs an independent installation. There is no shared Crow backend, required Docker installation, Marketplace listing, or separately billed API-key fallback.

## Start

The hosted installer is live at `birdapp.dev`. Install with:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh
```

The installer downloads and verifies the Linux binary, installs a permanent command, and offers to start setup. Downloads are public; access to Crow's private source repository is not required. The executable is compiled from Rust, so users do not need a language runtime or a checkout. See [installation](docs/user/install.md) for install-only mode and the source installation alternative.

Setup installs the executable and defaults to running both the connection service and worker on this machine. It guides Tailscale Funnel, creation or connection of your own GitHub App, repository selection, a separate Codex subscription login, and persistent systemd startup. Missing supported Linux dependencies can be installed with your confirmation. Browser steps print URLs you can open on a separate desktop. No browser is required on the host.

One GitHub App connects to one Crow service. You can assign different repositories to different workers; each worker receives a read-only GitHub token for the repository it is reviewing. See [connecting an existing App](docs/user/networking.md#connect-an-existing-github-app).

Setup does not run a test review. It checks connections, authentication, runtime capabilities, and startup configuration. Rerun `crow setup` to continue incomplete onboarding.

```sh
crow status
crow doctor --runtime
crow logs
```

Commands show summaries and next steps in plain text. Use `crow <command> --help` for explanations and examples, or `--format json` for structured results in scripts.

See the [Linux quickstart](docs/user/quickstart.md), [HTTPS choices](docs/user/networking.md), [separate-machine setup](docs/user/split-machines.md), and [configuration and operations](docs/user/operations.md).

## Review behavior

- New PR events trigger work. Crow does not poll GitHub for PRs on a schedule.
- Repository policies initially allow only your PRs. You can add authors or allow everyone. Authorized fork PRs targeting an enrolled repository are included.
- Drafts and the initial open backlog are skipped. Startup/recovery catch-up repairs missed work, with large batches held for release.
- Each worker allows three simultaneous reviews. Each review can use up to eight subagents. Both limits are configurable.
- Reviews inspect files and diffs through dedicated read-only tools. They cannot run repository tests, scripts, or dependency installation.
- Findings are advisory. Crow does not approve, request changes, or block merging.
- Retries resume saved work where possible. The default is ten retries separated by at least five seconds. Reports publish only after completion and validation.

Optional review instructions belong in `.crow/review.md`. Crow also reads applicable `AGENTS.md` files from the pinned target branch. These instructions cannot alter author authorization or permit execution of repository code.

## Development

Ordinary Cargo builds use the host toolchain. Release packaging additionally needs `musl-tools` and `binutils` to build and verify static Linux executables.

Install Rust with rustup and the Linux C build tools needed to compile bundled SQLite. `rust-toolchain.toml` selects the tested compiler and tools. Git is required for repository inspection.

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release --bin crow
bash scripts/build-release.sh
```

Application code lives in `src/`. One executable runs the CLI, connection service, worker, and private inspection MCP subprocess. SQLite is bundled; HTTPS uses rustls. The official Codex CLI remains a separate executable. `cargo run --bin crow -- help` lists commands. Source and downloaded installations use the same native executable lifecycle.

See the [Rust migration decisions](docs/design/rust-migration.md), [binary release design](docs/design/binary-release.md), and [runtime validation](docs/design/runtime-validation.md).

Tests use local fixtures and simulated provider/GitHub responses. They do not publish comments or run provider inference. Live subscription refresh, provider behavior across interruptions, and networking account authorization still require validation on an enrolled installation. Capability checks do not prove those live behaviors. See [runtime validation](docs/design/runtime-validation.md) for what the actual CLI probes establish.

The accepted product behavior and implementation sequence are in [the design documents](docs/design/implementation-plan.md).
