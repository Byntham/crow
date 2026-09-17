# Native binary releases

Crow releases contain a Rust executable with bundled SQLite and rustls for HTTPS. Git, Codex, and the selected ingress client remain separate programs. Linux x64 and arm64 release executables statically link musl, so their startup does not depend on the build host's glibc version. Ubuntu 20.04 and Debian 11 remain supported. Guided installation supports glibc distributions because the separately installed tools have their own platform requirements. Alpine, macOS, and Windows remain outside the supported installation configuration.

## Build and release

On the target architecture with the pinned Rust toolchain, a C compiler, `musl-tools`, and `binutils`:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
bash scripts/build-release.sh
```

Cargo embeds the version from `Cargo.toml`. The private `_inspection-mcp` command runs from the same binary, without extracting code or locating a language runtime. Release builds use thin link-time optimization and strip debug symbols. `scripts/build-release.sh` selects `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` and uses the native musl C compiler for SQLite and ring. Ordinary `cargo build` and source installation can still use the host GNU target.

`dist-release` receives `crow-vVERSION-linux-x64.tar.gz` or `crow-vVERSION-linux-arm64.tar.gz`, plus `SHA256SUMS` and build metadata. Archives contain `crow` and `THIRD_PARTY_NOTICES`. Each architecture builds and tests natively in GitHub Actions. Version tags must match `Cargo.toml`.

Builds produce private workflow artifacts. A separate manual publication workflow verifies the selected build's source and version, uploads immutable objects, verifies public downloads, and updates `latest.txt` last. Website deployment is also manual. See [hosted distribution](../maintainer/hosted-distribution.md).

## Runtime verification

Native integration tests start the executable using temporary configuration and real SQLite, check CLI and MCP behavior, and stop it through Linux signals. They do not require GitHub credentials or provider inference. Release packaging validates ELF architecture, archive entries, checksums, and the executable's reported version. Both packaging and publication reject executables with an ELF interpreter or shared-library dependencies. CI independently checks `readelf` output for dynamic dependencies and versioned glibc imports, then runs the packaged executable's version and help commands in an empty chroot with no libc or dynamic loader. This prevents a newer build runner from raising the runtime libc requirement.

Live GitHub onboarding, subscription authentication, and external ingress authorization still require an enrolled installation. Local fixtures cannot establish those account-dependent behaviors.
