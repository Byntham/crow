# TypeScript migration

Crow uses TypeScript for application code, pnpm for development, and Node 24 LTS for the installed runtime. The migration preserves the review workflow and keeps installation independent of development tools.

## Why TypeScript

Crow coordinates review states, worker ownership, saved sessions, retries, configuration, and GitHub publication. Shared types make those contracts explicit and let the compiler catch incompatible changes across modules. They also give a future provider implementation a defined interface.

Types do not validate webhooks, provider responses, configuration files, backups, or SQLite records. External values enter as `unknown` and need runtime checks before application code uses them. Existing validation remains part of the implementation. Concurrency and recovery behavior still need behavioral tests.

## Source and module format

Application modules in `lib/` and CLI entry points in `bin/` use `.mts`. TypeScript 7 checks these with `strict`, `module: NodeNext`, `moduleResolution: NodeNext`, `verbatimModuleSyntax`, and `erasableSyntaxOnly`. The project pins the compiler and Node 24 type definitions as development dependencies.

Imports retain their emitted `.mjs` paths. NodeNext resolves these to `.mts` source during type checking, and Node resolves them to JavaScript at runtime. Shared domain types live in `lib/types.mts`; modules use type-only imports where appropriate. Type definitions describe the data each operation needs without turning runtime validation into unchecked assertions.

Only erasable TypeScript syntax is allowed. For development and source installation, Node's `stripTypeScriptTypes` API removes annotations and type-only declarations without transforming application behavior. That build emits `.mjs` files with the same module layout and no runtime loader. Binary release builds separately bundle the application with esbuild and embed it in the official Node runtime.

Build scripts, behavioral tests, and runtime probes remain `.mjs`. These scripts orchestrate installation and exercise public behavior. Keeping them executable with Node alone lets users run installation and update checks without pnpm or a compiler. Tests import the built application, which also checks that emitted module paths work.

Compile-only checks in `test/types.test-d.mts` verify that publication requires a completed report and comparison, persisted record kinds cannot be mixed, repository settings exclude worker credentials, and configured subagents require model and reasoning selections. These checks fail if the relevant type contracts become too permissive.

## Migration sequence

1. Establish compiler settings, shared domain types, and a build that emits application modules into a separate directory.
2. Convert module groups along their existing boundaries. Add explicit function contracts, narrow caught errors, and validate parsed data while preserving behavior.
3. Point tests, probes, subprocess entry points, and the package CLI at the generated JavaScript. Keep provider and inspection subprocess paths valid in both development and installed releases.
4. Build installation releases from source in fresh staging directories. Check emitted syntax and CLI startup before switching the active release.
5. Run strict type checking, existing behavior tests, build and installer tests, and the actual Codex runtime probes. Compare failures against the existing behavior before changing review semantics.

Independent module groups can be migrated in parallel. Shared types and compilation diagnostics coordinate their integration. The conversion does not change database formats, review defaults, account requirements, or deployment roles.

## Developer workflow

```sh
pnpm install --frozen-lockfile
pnpm typecheck
pnpm build
pnpm build:binary
pnpm check
pnpm test:runtime
```

`pnpm typecheck` checks application source without emitting files. `pnpm build` checks types and emits fresh output into `dist/`. `pnpm check` checks types, rebuilds, checks script syntax, and runs the behavioral tests. `pnpm test` performs the build and behavioral checks without the separate type check. `pnpm test:runtime` checks and builds before exercising the official Codex executable against local fixtures.

Generated `dist/` output is disposable and excluded from version control. Development commands rebuild it before running Crow or its tests. Edit the `.mts` source, not generated `.mjs` files.

## Installation

The recommended installation is now a prebuilt Linux binary. Users download and verify the archive, extract it, and run `./crow setup`. The executable includes the Node 24 LTS runtime. Setup installs it into Crow's managed release directory and configures the stable command and systemd service. Users do not need source files, Node, pnpm, or TypeScript. See [binary releases](binary-release.md).

Source installation remains available through `bash scripts/install.sh` followed by `crow setup`. That installer selects or installs Node 24, strips source into a fresh staging directory, validates generated syntax, and runs CLI help before activating the release. It uses the checkout's source rather than an existing `dist/` directory. Installed modules live in the release's `bin/` and `lib/` directories, and the CLI wrapper and systemd service run those ordinary JavaScript modules. This path also needs neither pnpm nor TypeScript. Strict type checking is a development and release check.

See [runtime validation](runtime-validation.md) for the automated checks and the account-backed validation that remains separate from installation.
