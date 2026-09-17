# Runtime validation

The Rust implementation uses native tests with temporary files, real SQLite and Git, loopback HTTP fixtures, and simulated Codex processes. Run:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The original TypeScript baseline passed 321 tests, with five skipped, before migration. The Rust tests exercise the new implementation; the historical baseline is not proof that the port preserves every behavior.

The ordinary suite currently passes 211 tests. It checks durable receipts and jobs, separate worker/admin authentication, inspection permissions, merge-base comparisons, recovery, report validation, provider policy, setup callbacks, encrypted backups, installation, and retention. Linux subprocess tests exercise cancellation and cleanup. Setup prompt tests keep stdin open while sending SIGINT and SIGTERM after registration listeners have been dropped; both the prompt and its runtime must exit without waiting for EOF.

Opt-in installed-Codex tests use temporary synthetic authentication and a loopback Responses API. They must not use an operator's live account or perform model inference:

```sh
cargo test --locked --test runtime_probe -- --ignored --test-threads=1
CROW_PROBE_FIXTURE_TOKEN=fixture cargo test --locked --test runtime_probe -- --ignored --test-threads=1
```

The first command uses synthetic subscription authentication. The second uses a temporary proxy configuration with an environment-based credential and verifies bearer authentication for parent and delegated requests. Set `CROW_PROBE_CODEX` to select an installed Codex executable. Both modes keep their responses on localhost.

Provider event logs stream to disk without an aggregate stdout limit. Individual messages and inspection results remain bounded. Authentication, effective tool policy, selected models, and explicit session identity must be checked for new and resumed reviews.

Fixtures and installed-runtime probes do not establish real subscription entitlement, simultaneous production authentication refresh, review quality, or successful GitHub and ingress onboarding. Those require an enrolled installation. `crow setup` does not publish a test review. The live validation below used an existing account proxy; it does not establish direct subscription authentication or concurrent token refresh.

To repeat the executable lifecycle checks against an optimized build:

```sh
scripts/build-release.sh
CROW_TEST_BINARY="$PWD/target/x86_64-unknown-linux-musl/release/crow" \
  cargo test --locked --test native_cli --test human_cli --test inspection_mcp_port
```

## Live installation validation

On September 16, 2026, the existing Linux x64 installation was backed up and its Node service stopped. The native 0.3.0 executable completed interactive setup using the existing GitHub App, repository policies, account proxy, and Tailscale Funnel route. Only one Crow service ran during validation. Existing paused reviews were preserved.

[Testbed PR #5](https://github.com/Byntham/testbed/pull/5) exercised real GitHub webhooks and model inference. Its initial commit deliberately contained an incorrect percentage discount and an off-by-one pagination offset. Crow published an advisory review with both defects anchored to the correct source lines. Local fixture tests independently reproduced both failures. After correcting the functions, both tests passed and a synchronize webhook queued the new commit. Crow published a clean review with links to the earlier findings.

The flow also checked drain and undrain, queued pause/resume without a provider session, rejection of invalid resume settings without changing job state, interruption and continuation of the same saved Codex session, PR-comment pause/resume, and service restart followed by explicit resume. Both completed reviews retained their original session identities.

Live testing found and fixed GitHub delivery IDs exceeding the inherited JavaScript safe-integer limit. Delivery IDs now use exact unsigned 64-bit integers. CLI start/restart now wait for the native readiness signal, and successful doctor checks summarize capabilities and job counts instead of dumping history and help output.

The final optimized binary passed installation/MCP smoke checks and all three executable lifecycle tests. `crow doctor --runtime` passed against the installed binary, including public HTTPS, App permissions, pairing, proxy authentication, Codex capabilities, and enabled systemd startup. Restart followed immediately by status succeeded. The final startup logs had no delivery-audit error. The test PR was closed without merging, the original configuration was restored exactly, and the service was left active and undrained with only the two pre-existing paused reviews outstanding. A process scan found one native Crow daemon and no legacy Crow process.

PR review added regressions for bounded same-origin GitHub redirects without cross-origin credential forwarding, omitted guidance metadata remaining resumable and reconcilable after publication, and delegated resume/restart/final-save failures preserving retryable state. These tests use local HTTP fixtures, SQLite, and deterministic filesystem write failures.

Release builds use static musl executables to preserve compatibility with older supported glibc distributions. Packaging rejects dynamic loaders and shared-library dependencies. CI runs each architecture in an empty chroot. MCP subprocess regressions keep stdin or unread stdout open while sending SIGINT and SIGTERM, and cover large responses, request-size boundaries, and redirected files. Set `CROW_TEST_BINARY` for the installed-Codex probes to exercise the packaged MCP executable.

The final x64 static executable passed all 16 CLI, MCP, and service lifecycle tests, extracted-archive installation and checksum checks, and both installed-Codex probes in each synthetic authentication mode. Version and help also ran in an empty root under PRoot; the previous GNU executable failed the same check because its loader was absent. Native CI uses chroot for this check.
