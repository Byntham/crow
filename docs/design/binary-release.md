# Native binary releases

Crow releases contain one executable with the official Node 24 LTS runtime embedded through Node's single executable application support. Users do not need a source checkout, Node, pnpm, or a TypeScript compiler. Git, Codex, and the selected network ingress remain separate programs because Crow invokes them as independent tools.

The initial targets are Linux x64 and Linux arm64. These builds use glibc and the system C++ runtime, matching the official Node Linux distribution requirements. They are suitable for supported Ubuntu hosts such as gibo. Alpine and other musl distributions are not supported by these artifacts. macOS and Windows binaries are outside this release configuration.

## Build and release

On the target architecture, using the supported official Node 24 runtime:

```sh
pnpm install --frozen-lockfile
pnpm check
pnpm build:binary
```

The build bundles the TypeScript entry points with esbuild, creates a Node SEA preparation blob, and injects it into a copy of the same Node executable with postject. The runtime's `node:sqlite` implementation stays built in. Version metadata is a SEA asset. The executable dispatches its inspection MCP subprocess through a private command, so neither entry point requires extracted JavaScript files.

`dist-release` receives `crow-vVERSION-linux-x64.tar.gz` or `crow-vVERSION-linux-arm64.tar.gz`, plus `SHA256SUMS`. Each archive contains `crow` and `THIRD_PARTY_NOTICES`, including the embedded Node runtime's complete license notices. Use `node scripts/build-binary.mjs --out DIRECTORY` to choose another output directory. Build each architecture separately, then combine the checksum files when assembling a release.

The GitHub workflow builds and tests both architectures natively. A version tag must match `package.json`. Successful tag builds create a **draft** GitHub release for maintainer review. A manual workflow run creates downloadable build artifacts without creating a release. Adding this workflow does not itself publish Crow.

## Runtime safeguards and verification

The SEA configuration disables runtime flag extensions, so `NODE_OPTIONS` cannot inject a module or change runtime flags when Crow starts. Snapshots and V8 code caches are disabled. The same Node executable generates and receives the preparation blob.

The packaged smoke tests run with an empty executable search path and a deliberately invalid `NODE_OPTIONS`. They verify help and version output, the embedded inspection MCP entry point, and a real SQLite service startup and shutdown. The service test uses temporary local configuration and no GitHub App or Codex account.

```sh
mkdir -p /tmp/crow-binary-check
tar -xzf dist-release/crow-v*-linux-x64.tar.gz -C /tmp/crow-binary-check
CROW_TEST_BINARY=/tmp/crow-binary-check/crow node --test test/binary.test.mjs
```

The uncompressed executable includes the Node runtime and is approximately 127 MB before application code. The compressed release download is smaller. The build and smoke tests verify local behavior; real GitHub onboarding and subscription authentication still require account-level testing.
