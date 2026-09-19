# Run applications during reviews

Crow can discover how a project runs, prepare its dependencies, run tests and browser experiments, and inspect screenshots. Developers do not need to supply testing instructions on each PR. Crow reads existing manifests, CI configuration and documentation, chooses an investigation, and retries setup when the logs reveal a fixable problem.

## Enable automatic environments

The worker needs local rootless Podman with user namespaces, seccomp and delegated cgroup v2 controllers. This is a one-time worker prerequisite, not a task for each repository's developers. Existing installations remain inspection-only until the worker operator enables execution.

Enable automatic environments for all repositories assigned to the worker:

```sh
crow config worker.execution '{"automatic":true}'
crow doctor --runtime
crow service-restart
```

No image IDs, test commands, or new budget settings are required. Crow provisions a shared toolchain image itself. It currently contains Node/npm, Python/pip, Rust/Cargo, Go, C/C++ build tools, Chromium/Playwright, SQLite and PostgreSQL tools. The image is based on Alpine 3.23, whose stock packages provide Rust 1.91 and Node 24 while retaining Python 3.12. Package patch versions can change when the image is rebuilt. Projects requiring other versions or platforms may still need a custom image. Repository Dockerfiles are evidence for setup, never instructions to execute on the host.

Alternatively, enable selected repositories with automatic images:

```sh
crow config worker.execution '{"repositories":{"owner/repo":{}}}'
```

An entry without `image` uses `"image":"auto"`. Existing entries specifying a local immutable `sha256:` image ID still work. Those images must include the tools the project needs; dependency preparation through the package gateway requires Python 3, GNU tar, and `/opt/crow/proxy.py` from Crow's runtime image. `podman` can select a local executable path. These are local worker settings; a PR or connection-service job cannot grant execution authority.

## What Crow does

The reviewer discovers manifests and relevant CI/documentation, then selects setup commands. Discovery offers candidates for Node, Python, Rust and Go projects, including nested projects. These candidates are not promises that a particular command is correct. The reviewer inspects the project and adapts them. Discovery prioritizes root and shallow manifests, reports omitted projects in very large repositories, and honors exact npm/pnpm/Yarn versions declared by `packageManager`, including modern Yarn.

Rust discovery includes `rust-toolchain` and `rust-toolchain.toml` files from the project and its ancestors. The stock Alpine `rustc` and `cargo` binaries do not enforce those files' version pins. The reviewer must check the installed compiler against the manifest's minimum Rust version and any exact toolchain requirement. Testing with a compatible stock compiler does not verify the exact pinned compiler. If an exact version is unavailable, an operator-provided image containing it is required; Crow does not automatically download replacement Rust toolchains.

`prepare_environment` runs installation in a fresh sandbox at a pinned revision. It saves the resulting workspace only after successful preparation. Dependencies and caches must stay under `/workspace`; `HOME` is `/workspace/.crow-home`. Environment variables exported in one setup shell do not persist into later test shells, so test commands must activate a virtual environment or use explicit tool paths when needed.

Preparation can download packages through a restricted HTTPS gateway. The container still has no external network interface. A private Unix socket connects its loopback proxy to Crow's gateway, which accepts only selected public package hosts on port 443. Before connecting upstream, it requires a valid TLS ClientHello whose server name matches the requested package host. Missing names, mismatches and encrypted ClientHello extensions are rejected. It then rejects private and special IP addresses after DNS resolution and connects to the validated address. It does not forward worker credentials. Direct connections to unlisted hosts, private registries and production services are unavailable.

Crow checks the container's connection to that socket before running setup and reports access failures at the package gateway startup stage. The private socket mount requests a per-container SELinux label (`:ro,Z`); Crow does not disable container labeling. On SELinux-enforcing hosts, policy must also permit the container to connect to the worker's Unix socket. Relabeling the directory alone may not permit this. Enforcing SELinux configurations have not been validated, so dependency preparation on those hosts is not yet claimed as supported. Offline experiments do not mount or use this socket.

The package hosts currently cover npm/Yarn, PyPI, crates.io, the Go module proxy and checksum service, Maven Central and RubyGems. Inclusion of a package host does not mean every language toolchain is installed. Requests, concurrent connections and connection lifetimes are bounded. The gateway carries end-to-end HTTPS tunnels. It checks the CONNECT destination, resolved IP addresses and initial TLS server name, but cannot inspect encrypted HTTP Host headers, request paths or uploads. An allowed service that supports HTTP domain fronting may still route requests to another tenant. This is a restricted dependency network, not a package vulnerability scanner or a guarantee against data uploads. ClientHello inspection is limited to 64 KiB, 16 TLS records and five seconds; clients that require encrypted ClientHello are unsupported.

`run_experiment` restores the selected prepared workspace into a fresh, offline container. It verifies that the environment belongs to the exact requested commit and current image, then restores tracked source from that commit over the dependency snapshot. Setup hooks cannot silently replace the tracked source under test. The download socket is absent. Crow can run existing tests, write temporary reproductions, start services and exercise them through loopback. It compares equivalent experiments on base and head before attributing failures to the change.

Identical preparations can be reused within the same review, at the same commit and image. These full workspace snapshots are not shared across reviews. Their tracked source is restored from Git before each test. Crow never extracts repository or cache archives on the host.

## Dependency reuse and cleanup

Across PR updates, Crow can reuse verified package downloads while installing the new revision into a fresh workspace. The download cache supports npm SHA-512 content blobs, Cargo archives pinned by `Cargo.lock`, and Go archives/module files pinned by `go.sum`. It excludes registry metadata, extracted dependency source, generated application files, and build outputs. Python and Yarn/pnpm-specific stores are not shared; those environments still work, but may download again. Cache selection uses pinned manifests and lockfiles, repository, PR, base/head side, and toolchain image. Changes to dependencies invalidate the entry. Repositories that track `.crow-home` skip this optimization.

A cache failure is a warning and preparation falls back to normal installation. Imports verify each package again and reserve workspace capacity for source and installation. The download cache has a 4 GiB storage allowance shared by reviews, with entries expiring after seven days without use. Reads refresh their access time; inserts and pruning are serialized. Periodic maintenance runs even when no new test is starting. Old full-workspace cache entries are removed during migration.

Containers, services, browsers and temporary workspaces are removed after each experiment. After Crow saves and submits a valid report, it releases that review's prepared workspace snapshots. Completed, superseded and cancelled jobs also release snapshots during maintenance. Paused and active reviews retain them for resumption. Receipts, logs and screenshots follow the normal configurable seven-day review retention.

Maintenance runs at worker startup, hourly, and after reviews finish. It retries stopped-container cleanup, removes owned temporary files left by crashes, and prunes old Crow-managed image tags after seven unused days. Current images, images needed by resumable reviews, and explicitly configured image IDs remain protected. Cleanup touches only resources recorded by this Crow installation; it never runs a global Podman prune. Unregistered images from older Crow versions are left alone because ownership cannot be proved. PR closure follows the normal cancellation/terminal cleanup path, so storage cleanup does not depend on a PR eventually merging.

When cleanup fails, Crow retains the ownership receipts needed to retry instead of forgetting the leftover resources. `crow status` flags maintenance warnings; `crow status --format json` includes the details. `runtime-maintenance.json` in the data directory records the last cleanup result, and `crow cleanup` runs maintenance explicitly.

## Status in the main comment

Runtime tests are selective. The worker operator must enable execution, and the reviewer chooses useful checks based on the diff, project manifests, CI and available dependencies. Crow instructs the reviewer to investigate changed behavior autonomously, but does not require a test command on every review. Documentation-only changes can need no runtime check; unsupported platforms or private dependencies can prevent one.

The main Crow status comment shows runtime testing separately from review progress. It reports disabled execution, a pending testing decision, environment setup, running tests, and final command counts. Setup attempts have their own counts. If the reviewer finishes without running a test command, the comment says so. If setup fails before any tests run, it reports that testing was blocked. Interrupted jobs never leave a command labelled as still running.

Counts come from saved execution receipts, not the model's description of its work. Workers send updates on their ten-second heartbeat; very short experiments may appear only as completed counts. The service updates the existing comment when those counts change. Counts include both base and head commands and unsuccessful investigation attempts, so a failed command does not itself mean the PR introduced a bug. The review explains the evidence and coverage gaps. See [comment examples](../validation/runtime-status.md).

## Visual investigations

The managed image supplies Playwright at `/opt/browser/node_modules/playwright-core/index.mjs` and Chromium at `/usr/bin/chromium`. Launch Chromium with `--no-sandbox --disable-dev-shm-usage` inside Crow's constrained outer container. The reviewer can navigate the running application, interact with it and capture screenshots using ordinary Playwright commands.

An experiment can request up to three PNG artifacts by absolute container path. Crow exports and validates each image before removing the container. Each image must be under 4 MiB and no larger than 4096 pixels in either dimension. `read_artifact` returns actual MCP image content, with the generating command and commit, so a vision-capable reviewer can inspect it. DOM and accessibility checks alone do not establish visual correctness.

Artifacts and receipts remain in the worker's review directory and follow normal review retention. They are not automatically uploaded to GitHub. A missing or invalid artifact is reported explicitly without converting a successful test command into a claim of visual verification.

## Limits and failures

Existing experiment limits still apply. Defaults are 120 seconds per setup/test command, 1 GiB of RAM, 512 MiB for each writable filesystem, two CPUs, 128 PIDs, and 12 attempts per review. Existing overrides still work; no new budget configuration is needed. First-time toolchain provisioning has a separate internal 30-minute ceiling and remains subject to review cancellation and the existing review timeout. It is recorded separately as `provisionMs` so a cold image build does not consume the command's runtime deadline.

Execution receipts record the current stage and where failures occurred, such as runtime availability, image preparation, source restoration, setup commands, test commands, exports, or cleanup. The main comment and final receipt table show stage names and warnings. Detailed bounded errors and command logs stay in `reviews/<job-id>/experiments/<experiment-id>.json` on the worker; they are not copied into public status comments. Setup receipts also retain a bounded list of package-gateway errors, including TLS rejections that package clients may describe only as connection failures. Successful commands remain successful if optional cache saving or screenshot collection fails, with those failures reported separately.

Captured output retains a bounded beginning and end with an omission marker, so lengthy logs retain their final diagnostics.

Every attempted setup or experiment counts toward the existing attempt limit, including cache hits and failures. Discovery and reading saved evidence do not. The main reviewer executes serially; delegated reviewers remain inspection-only. Resumes retain receipts and successful preparations.

Reports distinguish setup from test commands. Passing dependency installation does not mean tests passed. An environment failure, timeout, pre-existing test failure or missing screenshot must not be reported as proof of a PR regression. If Crow cannot repair the setup, it continues inspection and explains the concrete gap in runtime coverage.

Source and dependency code run only in rootless containers with no host workspace mounts, host credentials, Linux capabilities or privilege escalation. The root filesystem is read-only. During setup, the sole host mount contains the restricted download socket; tests have no host mounts. Memory, CPU, process count, scratch space, output and time are bounded. Crow verifies resource limits before extracting source. Cancellation stops the container; interrupted work is recovered on resume. Rootless containers share the host kernel, so workers reviewing hostile code should run on dedicated machines or VMs.

The image includes PostgreSQL client/server tools, but its current container user and dropped privileges prevent ordinary `initdb` server initialization. Their presence does not imply PostgreSQL-backed applications can be started.

Linux workloads are the first backend. Native macOS/Windows applications, GPUs, devices, private dependencies and authenticated external services can block runtime investigation. Source archives do not materialize Git metadata, submodules or LFS objects, and are limited to the configured workspace size, capped at 512 MiB. Crow reports those limits rather than treating untested behavior as verified.

To disable execution, replace the worker setting with `{"automatic":false,"repositories":{}}` and restart the service.
