# Native binary releases

Crow releases contain a Rust executable with bundled SQLite and rustls for HTTPS. Git, Codex, and the selected ingress client remain separate programs. Linux x64 and arm64 builds use glibc. Alpine, macOS, and Windows are outside the release configuration.

## Build and release

On the target architecture with stable Rust and a C compiler:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
bash scripts/build-release.sh
```

Cargo embeds the version from `Cargo.toml`. The private `_inspection-mcp` command runs from the same binary, without extracting code or locating a language runtime. Release builds use thin link-time optimization and strip debug symbols.

`dist-release` receives `crow-vVERSION-linux-x64.tar.gz` or `crow-vVERSION-linux-arm64.tar.gz`, plus `SHA256SUMS` and build metadata. Archives contain `crow` and `THIRD_PARTY_NOTICES`. Each architecture builds and tests natively in GitHub Actions. Version tags must match `Cargo.toml`.

Builds produce private workflow artifacts. A separate manual publication workflow verifies the selected build's source and version, uploads immutable objects, verifies public downloads, and updates `latest.txt` last. Website deployment is also manual. See [hosted distribution](../maintainer/hosted-distribution.md).

## Runtime verification

Native integration tests start the executable using temporary configuration and real SQLite, check CLI and MCP behavior, and stop it through Linux signals. They do not require GitHub credentials or provider inference. Release packaging validates ELF architecture, archive entries, checksums, and the executable's reported version.

Live GitHub onboarding, subscription authentication, and external ingress authorization still require an enrolled installation. Local fixtures cannot establish those account-dependent behaviors.
