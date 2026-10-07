# Runtime validation

The test suite uses temporary files, real SQLite and Git, loopback HTTP fixtures, and fake Codex and Claude Code executables compiled from `tests/fixtures/`. Run:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

It covers durable receipts and jobs, separate worker/admin authentication, inspection permissions, merge-base comparisons, recovery, report validation, provider policy for both providers, setup callbacks, encrypted backups, installation, and retention. Linux subprocess tests exercise cancellation and cleanup, including prompts and MCP servers that receive SIGINT or SIGTERM while stdin stays open.

## Runtime experiments

`tests/runtime_experiments.rs` drives the runtime tools against a fake Podman. It covers preparation and reuse, fresh runs, base comparisons, the isolation flags on every container, the setup-only gateway mount, the attempt budget, recovery after a crash, and the report summary. Real isolation needs rootless Podman and network access to build the runtime image:

```sh
cargo test --locked --test runtime_podman -- --ignored --test-threads=1
```

That test checks the non-root user, empty capabilities, `no_new_privs`, that `unshare`, `clone` and `clone3` cannot create a user namespace while threads still work, the read-only root, the lack of network in tests, the gateway's allowlist, offline use of a prepared dependency, and container removal.

## Installed Codex probes

Opt-in tests run the installed Codex executable against temporary synthetic authentication and a loopback Responses API. They never use an operator's account or perform model inference:

```sh
cargo test --locked --test runtime_probe -- --ignored --test-threads=1
CROW_PROBE_FIXTURE_TOKEN=fixture cargo test --locked --test runtime_probe -- --ignored --test-threads=1
```

The first command uses synthetic subscription authentication. The second uses a temporary proxy configuration with an environment-based credential and verifies bearer authentication for parent and delegated requests. Set `CROW_PROBE_CODEX` to select an executable, and `CROW_TEST_BINARY` to exercise a packaged Crow executable as the MCP server.

## Claude Code checks

Claude Code 2.1.289 was checked directly against a subscription login, with Haiku and a throwaway repository served by Crow's inspection MCP server:

- The `system/init` event reports `session_id`, the offered `tools`, `mcp_servers` with connection status, the resolved `model`, `permissionMode`, and `apiKeySource` before the first model request. With Crow's flags, the tools were exactly the inspection tools plus `StructuredOutput`.
- `--json-schema` returns the report as `structured_output` on the final `result` event.
- `--resume` keeps the same session ID.
- Without a login, the assistant event carries `error: "authentication_failed"` and the result has `is_error: true`.
- `claude auth status --json` reports `loggedIn`, `authMethod` (`claude.ai`, `oauth_token`, or `api_key`), and `apiProvider`.
- The SDK `initialize` control request returns the model catalog and account without starting a conversation, including when stdin closes immediately after the request.
- An empty `--setting-sources` and a `--settings` overlay are accepted.

A full review through an enrolled installation, concurrent token refresh, and delegated child reviews with Claude Code have not been exercised live.

## Release executables

```sh
scripts/build-release.sh
CROW_TEST_BINARY="$PWD/target/x86_64-unknown-linux-musl/release/crow" \
  cargo test --locked --test native_cli --test human_cli --test inspection_mcp_port
```

Release builds are static musl executables. Packaging rejects dynamic loaders and shared-library dependencies, and CI runs each architecture's executable in an empty chroot.

## Live installation validation

On September 16, 2026, an existing Linux x64 installation completed interactive setup with the 0.3.0 executable, reusing its GitHub App, repository policies, Codex account proxy, and Tailscale Funnel route.

[Testbed PR #5](https://github.com/Byntham/testbed/pull/5) exercised real GitHub webhooks and model inference. Its first commit contained an incorrect percentage discount and an off-by-one pagination offset; Crow published an advisory review with both defects anchored to the correct lines. After the fix, a synchronize webhook queued the new commit and Crow published a clean review linking the earlier findings. The run also checked drain and undrain, pause and resume with and without a provider session, rejection of invalid resume settings, interruption and continuation of the same Codex session, PR-comment commands, and restart followed by resume. `crow doctor --runtime` passed against the installed binary.

That validation used an account proxy, so it does not establish direct subscription authentication or concurrent token refresh. Fixtures and probes do not establish subscription entitlement, review quality, or GitHub and ingress onboarding for a new account; those need an enrolled installation. `crow setup` never publishes a test review.
