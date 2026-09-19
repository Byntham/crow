# Autonomous runtime investigation

The initial runtime implementation required operators to build dependency images and give reviewers setup instructions. The next implementation moves discovery, image provisioning and dependency installation into Crow's review workflow. Existing worker authorization and execution limits remain in force.

`runtime.rs` discovers manifests from pinned Git objects and provides candidate setup/test commands. The reviewer reads CI and project documentation to refine these candidates. Discovery does not run code or assume that a generated command has succeeded. Crow builds only its embedded Containerfile and proxy helper, never a repository Dockerfile, and records the resulting immutable image ID.

`downloads.rs` owns a restricted CONNECT gateway. A setup container has only loopback networking and a read-only mount containing a private Unix socket. A Python bridge exposes an HTTP proxy on container loopback. The worker gateway validates the CONNECT host and port, checks that a valid initial TLS ClientHello names the same host, rejects missing SNI and encrypted ClientHello, then resolves and validates all addresses and connects to one of those exact addresses. It buffers at most 64 KiB across 16 handshake records for five seconds before rejecting the handshake. rustls validates the ClientHello; a bounds-checked extension scan rejects ECH. The validated bytes are forwarded unchanged. It has no host credential forwarding. HTTPS tunnels reach selected public registry endpoints. TLS stays end-to-end, so encrypted HTTP Host headers, paths and uploads are not inspected. Allowed services may permit uploads or HTTP domain fronting to another tenant; the SNI check does not guarantee HTTP tenant identity. Connection count, bytes and time are bounded. At most 16 tunnels are active and one accepted socket can wait for admission. Only while a socket is waiting, 15 seconds of inactivity in both directions releases an established tunnel. This reclaims idle keep-alive connections, but encrypted traffic means a slow first response can also be interrupted. Traffic resets the inactivity deadline; uncongested connections retain the 120-second whole-connection limit. The listener and active tunnels are cancelled together.

`execution.rs` runs setup in a disposable container, saves a bounded tar stream without host extraction, and reuses successful preparations within one review by exact commit, image and setup command. A prepared environment is authorized only for the same commit and selected image. Experiments restore it into another container without the socket or proxy configuration, then overwrite tracked source with the pinned Git archive to undo setup-hook edits. Setup failures stay distinguishable from test failures in durable receipts and deterministic report rows.

Dependencies install fresh for each review. Package-manager caches, installed dependencies and build outputs never cross review boundaries. Repository-controlled integrity fields can cause package managers to accept substituted cached bytes, so checksums alone do not justify sharing writable caches. Successful preparations remain reusable within the same review through prepared workspace snapshots. The managed toolchain image is shared. `runtime_cache.rs` only removes obsolete cross-review caches from earlier versions.

`runtime_cleanup.rs` performs serialized maintenance at startup, hourly and after reviews finish. Prepared workspaces are released after an accepted report and for terminal jobs; active and paused jobs retain them. Stopped containers, temporary files and old managed image tags require recorded ownership. Image tags are scoped to the Crow data directory. Current images, resumable-review images and configured images are protected. Failed cleanup retains receipts for retries and writes an operator-visible maintenance result. Review logs and screenshots use normal retention.

`runtime_diagnostics.rs` persists typed stages before operations. Receipts distinguish command failures from screenshot and cleanup warnings. Public status includes trusted stage names and counts; bounded private errors stay in worker receipts. Output keeps the beginning and end of long logs.

Containers remain running while Crow copies out prepared workspaces and requested PNGs, then are removed. PNGs undergo bounded decoding before publication as MCP image content. Images are served by a dedicated `read_artifact` tool; ordinary tool output cannot smuggle arbitrary MCP content. Artifact reads retain receipt provenance. Artifact storage is local to the review and existing retention removes it.

The provider's review instructions require autonomous discovery and focused runtime investigation when execution is available. They require before/after visual evidence for UI investigations and distinguish setup, test and artifact outcomes. This is model-directed orchestration, not a deterministic guarantee that every changed path is tested. Live evaluations remain necessary.

## Validation

`tests/autonomous_runtime.rs` has a normal MCP image-transport test and five opt-in real-container scenarios. They cover npm/PyPI preparation and recovery, offline base/head tests, Chromium screenshots, concurrent downloads, fresh Rust and Go dependencies across reviews, hostile Python imports, and an updated PR that installs npm dependencies without old caches or installed/generated files. The updated-PR scenario also checks completed, paused and superseded workspace retention.

`tests/discovery_runtime.rs` verifies discovery and actual installation with pinned pnpm and modern Yarn. The opt-in `real_podman_cleanup` library test verifies removal of owned stopped containers and expired managed tags while preserving active/foreign containers and current images. See [cleanup and diagnostics validation](../validation/runtime-hardening.md) for the final results and failure-path coverage.

```sh
cargo test --locked
CROW_TEST_PODMAN=/absolute/path/to/podman cargo test --test autonomous_runtime -- --ignored --nocapture
```

The existing `execution_mcp` suite continues to check isolation, output bounds, deadlines, cancellation, abrupt interruption, recovery and reporting. Run tests sharing a local container store sequentially when first building images, especially with the VFS storage driver.

The live model fixture can be created without Crow-specific instructions:

```sh
python3 examples/visual_checkout/prepare.py --autonomous .crow-data/visual-demo
cargo run --example local_review -- .crow-data/visual-demo/source.json .crow-data/visual-demo/crow-run auto /absolute/path/to/podman
```

It requires installation of an ordinary npm dependency and includes a binary-only visual regression. `--clean` generates a changed graphic that preserves the label, for a false-positive control. The runner uses the operator's configured model and saves a local report; it does not publish to GitHub.
