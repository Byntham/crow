# Runtime cleanup and diagnostics audit

This follow-up replaces cross-review prepared-workspace sharing with verified package-download reuse. It also adds ownership-based maintenance and stage-specific failure reporting. Earlier model-driven and historical-PR results remain in their original validation documents; this audit tests the runtime and worker changes directly.

## Verified behavior

| Scenario | Result |
| --- | --- |
| A PR changes source without changing its npm lockfile | A new review installs with `npm ci --offline` using cached downloads. The new source is present; old generated files and installed dependencies are absent before installation. The installed package passes its probe. |
| Corrupted download-cache archive | Preparation succeeds through normal installation and records `cacheRestoreError`. |
| Cargo and Go package reuse | A second preparation imports the pinned `.crate`, `.zip` and `.mod` files, excludes Go's `.ziphash` metadata, and reports three verified files. Rust and Go tests run offline. |
| Finished versus paused reviews | Terminal jobs release prepared snapshots while retaining receipts. Paused jobs retain snapshots for resumption. Report submission also releases snapshots promptly. |
| Cleanup ownership | Real Podman removes an owned stopped container and an expired registered image tag. It preserves a running container, a foreign container and the current managed image. |
| Cleanup failure and concurrent maintenance | Regression tests preserve ownership receipts for retry, serialize maintenance, and verify retries after a busy lock. Shutdown joins the maintenance task. |
| Setup failure and repair | The real setup receipt identifies `setup_command`; a subsequent setup succeeds and offline tests run. |
| Runtime, source or container startup failure | Diagnostic tests distinguish their failure stages and avoid cleanup warnings for containers that never started. |
| Optional cache or screenshot failure/timeout | Completed commands keep their result. A required successful snapshot and already-collected images remain usable; unfinished screenshots are marked unsaved. |
| Screenshot directory cannot be created | A regression test records an artifact warning while preserving the successful command result. |
| Long output | A real failing command retains its final diagnosis after more than 40,000 output bytes. |
| Unsafe archives and modified cache contents | Helper tests reject symlinks, hardlinks, path traversal, registry metadata and packages that fail checksum verification. Imports respect a byte allowance. |
| Package-manager discovery | Actual pnpm 9.15.4 and Yarn 4.9.2 installs and offline test commands pass. Root/shallow projects are prioritized before the discovery cap. |
| Source restoration attacks | Real repository import collisions and Python user-site hooks do not replace Crow's isolated helper; tests receive pinned tracked source. |

## Local results

- 283 ordinary tests passed across all targets.
- Eight opt-in real-container tests passed: one MCP isolation/lifecycle suite, five managed-runtime scenarios, one package-manager discovery scenario and one ownership-based cleanup scenario.
- Formatting, `git diff --check` and clippy across all targets with warnings denied passed.

The real suites ran sequentially against rootless Podman 5.7. The managed image was `sha256:8438a8eaa6758d9f2d32a7b3501ee80134644de3bd1d3127df2f96f65ff8b01d`. The MCP suite used `sha256:c83674e1999044d33d751661371b873539f47e5b5c5ca3320c7e0377acca6238`.

Reproduce with a suitable local Podman installation and an immutable Alpine image for the MCP fixture:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
export CROW_TEST_PODMAN=/absolute/path/to/podman
export CROW_TEST_IMAGE=sha256:YOUR_LOCAL_ALPINE_IMAGE_ID
cargo test --locked --test execution_mcp -- --ignored --nocapture --test-threads=1
cargo test --locked --test autonomous_runtime -- --ignored --nocapture --test-threads=1
cargo test --locked --test discovery_runtime -- --ignored --nocapture --test-threads=1
cargo test --locked --lib real_podman_cleanup -- --ignored --nocapture --test-threads=1
```

The release workflow runs these real suites on x64. Its ARM job runs ordinary checks, builds and portable-binary smoke tests. Avoid concurrent Cargo builds in one target directory while subprocess tests are running: replacing a running test executable can make Linux `current_exe()` return a deleted path.

## Retention and remaining limits

Prepared snapshots stay within a review and are released when it finishes. Verified downloads have a shared 4 GiB allowance, a 512 MiB per-archive limit and seven-day idle expiry. Import reserves workspace capacity for pinned source and installation. Receipts, screenshots and logs follow configurable review retention, normally seven days. Paused reviews retain their prepared environments.

npm SHA-512 blobs, Cargo lockfile-pinned archives and Go sum-pinned archives/module files are eligible for cross-review reuse. Python and Yarn/pnpm-specific stores are not shared. Dependency changes invalidate the cache. Repositories tracking `.crow-home` skip caching.

Cleanup requires recorded ownership. Crow does not prune unrelated Podman storage or unregistered images left by older versions. Container-engine build storage is outside this ownership registry. Maintenance warnings appear in operator status; detailed failures remain in local receipts and `runtime-maintenance.json`. Public PR status exposes stage names and counts without raw host errors or command output.

These checks do not establish universal platform support. Private registries, unsupported toolchain versions and native mobile/desktop environments can still block an investigation. The managed PostgreSQL binaries cannot initialize an ordinary server under the current container user and capability policy. The earlier historical-PR coverage limitations still apply.
