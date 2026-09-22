# Validate runtime reviews

Run ordinary regression tests and static checks from the repository root:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
```

Real-container tests require rootless Podman, cgroup v2, seccomp, delegated resource controls, and package-download access. They fail rather than silently skip when explicitly requested without prerequisites. Run them sequentially, especially during first-time image provisioning:

```sh
podman pull docker.io/library/alpine:3.22
crow_image_id=$(podman image inspect --format '{{.Id}}' docker.io/library/alpine:3.22)
export CROW_TEST_IMAGE="sha256:${crow_image_id#sha256:}"
# Optional: export CROW_TEST_PODMAN=/absolute/path/to/podman
cargo test --locked --test execution_mcp -- --ignored --nocapture --test-threads=1
cargo test --locked --test autonomous_runtime -- --ignored --nocapture --test-threads=1
cargo test --locked --test discovery_runtime -- --ignored --nocapture --test-threads=1
cargo test --locked --lib real_podman_cleanup -- --ignored --nocapture --test-threads=1
```

The [release workflow](../../.github/workflows/release.yml) runs these suites on x64 under a delegated systemd user scope. Both architectures run ordinary tests and portable-binary checks. The suites cover MCP transport, isolation, cancellation and recovery, registry downloads, offline base/head tests, source restoration, browser images, package-manager discovery, and ownership-based cleanup. Avoid replacing an executable while subprocess tests are using it.

An installed-Codex probe uses synthetic localhost responses to exercise real experiment tools without live inference:

```sh
cargo test --locked --test runtime_probe runtime_executes_real_containers -- --ignored --nocapture
```

These tests do not measure whether a live model chooses good experiments. A passing shell command also does not prove every nested assertion passed. Inspect receipts and coverage claims when evaluating a real review.

## Archived model evaluations

The feature's development evaluations include a binary-only checkout defect, a clean visual control, a Python regression, real historical PRs including T3 Code and htmx, and Crow's self-reviews. The [reports, screenshots and limitations](https://github.com/Byntham/crow/tree/a0f240d0ceb3618559ea0e489f7445c703a8de2b/docs/validation) and [evaluation scripts](https://github.com/Byntham/crow/tree/a0f240d0ceb3618559ea0e489f7445c703a8de2b/examples) remain at that immutable commit. These are selected historical observations, not a benchmark or claims about the current build. Some describe mechanisms subsequently removed.

To recover the complete evaluation tooling and matching implementation without adding them to the current tree:

```sh
git fetch origin a0f240d0ceb3618559ea0e489f7445c703a8de2b
git worktree add --detach ../crow-runtime-evaluations a0f240d0ceb3618559ea0e489f7445c703a8de2b
```

In that worktree, follow the archived `examples/historical_reviews/README.md` or `examples/visual_checkout/README.md`. Replays use the operator's model account and inference billing but publish no upstream comments. Historical PR metadata or dependencies may have changed, so a replay is not guaranteed to reproduce the original environment. Keep new reports, receipts and screenshots under ignored `.crow-data/`, outside the source tree.
