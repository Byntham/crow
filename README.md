# Crow

Crow reviews GitHub pull requests on your own Linux machine, using your Codex subscription. It responds on GitHub with advisory inline findings and a summary intended for coding agents.

Each operator runs an independent installation. There is no shared Crow backend, required Docker installation, Marketplace listing, or separately billed API-key fallback.

## Start

The hosted installer is prepared for `birdapp.dev` but has not been deployed yet. Once published, install with:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh
```

The installer downloads and verifies the Linux binary, installs a permanent command, and offers to start setup. Downloads are public; access to Crow's private source repository is not required. Node 24 LTS is included, so users do not need Node, pnpm, or a checkout. See [installation](docs/user/install.md) for install-only mode and the source installation alternative.

Setup installs the executable and defaults to running both the connection service and worker on this machine. It guides Tailscale Funnel, creation of your own GitHub App, repository selection, a separate Codex subscription login, and persistent systemd startup. Missing supported Linux dependencies can be installed with your confirmation. Browser steps print URLs you can open on a separate desktop. No browser is required on the host.

Setup does not run a test review. It checks connections, authentication, runtime capabilities, and startup configuration. Rerun `crow setup` to continue incomplete onboarding.

```sh
crow status
crow doctor --runtime
crow logs
```

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

Use Node 24 LTS. The `packageManager` field in `package.json` pins pnpm to 12.3.4. See the [pnpm installation guide](https://pnpm.io/installation) if pnpm is missing. pnpm manages the development workflow; Crow runs on Node. End users do not need pnpm.

```sh
pnpm install --frozen-lockfile
pnpm typecheck    # Strict TypeScript checking
pnpm build        # Check types and emit JavaScript into dist/
pnpm build:binary # Check types and package this Linux architecture
pnpm check        # Check types, rebuild, and run the test suite
pnpm test:runtime # Actual installed Codex, with local synthetic responses
```

Application source lives in `lib/*.mts` and `bin/*.mts`. TypeScript 7 checks it in strict NodeNext mode. Node emits the runnable `.mjs` modules into `dist/`; `pnpm start` builds and runs `dist/bin/crow.mjs`. Release builds bundle the application into a Node single executable. Build scripts, behavioral tests, and runtime probes remain JavaScript; compile-only contract tests use TypeScript. `pnpm test` rebuilds and runs the suite without the separate type check; use `pnpm check` before submitting changes. See the [TypeScript migration design](docs/design/typescript-migration.md) and [binary release design](docs/design/binary-release.md).

Tests use local fixtures and simulated provider/GitHub responses. They do not publish comments or run provider inference. Live subscription refresh, provider behavior across interruptions, and networking account authorization still require validation on an enrolled installation. Capability checks do not prove those live behaviors. See [runtime validation](docs/design/runtime-validation.md) for what the actual CLI probes establish.

The accepted product behavior and implementation sequence are in [the design documents](docs/design/implementation-plan.md).
