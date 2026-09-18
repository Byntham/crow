# Autonomous runtime investigation

The initial runtime implementation required operators to build dependency images and give reviewers setup instructions. The next implementation moves discovery, image provisioning and dependency installation into Crow's review workflow. Existing worker authorization and execution limits remain in force.

`runtime.rs` discovers manifests from pinned Git objects and provides candidate setup/test commands. The reviewer reads CI and project documentation to refine these candidates. Discovery does not run code or assume that a generated command has succeeded. Crow builds only its embedded Containerfile and proxy helper, never a repository Dockerfile, and records the resulting immutable image ID.

`downloads.rs` owns a restricted CONNECT gateway. A setup container has only loopback networking and a read-only mount containing a private Unix socket. A Python bridge exposes an HTTP proxy on container loopback. The worker gateway validates the destination host and port, resolves and validates all addresses, then connects to one of those exact addresses. It has no host credential forwarding. HTTPS tunnels permit package-manager traffic to selected public registry hosts, not arbitrary internet access. Connection count, bytes and time are bounded. The listener and active tunnels are cancelled together.

`execution.rs` runs setup in a disposable container, saves a bounded tar stream without host extraction, and caches successful preparations by repository location, commit, image and setup command. A prepared environment is authorized only for the same commit and selected image. Experiments restore it into another container without the socket or proxy configuration, then overwrite tracked source with the pinned Git archive to undo setup-hook edits. Setup failures stay distinguishable from test failures in durable receipts and deterministic report rows.

Containers remain running while Crow copies out prepared workspaces and requested PNGs, then are removed. PNGs undergo bounded decoding before publication as MCP image content. Images are served by a dedicated `read_artifact` tool; ordinary tool output cannot smuggle arbitrary MCP content. Artifact reads retain receipt provenance. Artifact storage is local to the review and existing retention removes it.

The provider's review instructions require autonomous discovery and focused runtime investigation when execution is available. They require before/after visual evidence for UI investigations and distinguish setup, test and artifact outcomes. This is model-directed orchestration, not a deterministic guarantee that every changed path is tested. Live evaluations remain necessary.

## Validation

`tests/autonomous_runtime.rs` has a normal MCP image-transport test and an opt-in real-container test. The latter provisions the image, downloads npm and PyPI dependencies, exercises failed setup and repair, checks snapshot reuse, revision mismatch rejection and restoration of source edited by setup hooks, runs a base/head regression offline, and exports a real Chromium screenshot. It also checks missing artifacts. A second real-container test downloads Cargo and Go dependencies, restores their caches, and compiles and tests both projects offline.

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
