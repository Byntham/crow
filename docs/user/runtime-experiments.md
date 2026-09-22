# Run applications during reviews

Crow can discover how a project runs, prepare its dependencies, run tests and browser experiments, and inspect screenshots. Developers do not need to supply testing instructions on each PR. Crow reads existing manifests, CI configuration and documentation, chooses an investigation, and retries setup when the logs reveal a fixable problem.

## Normal setup and disabling tests

Runtime testing is enabled by default. The reviewer decides which changes benefit from running tests or browser checks; developers do not need to add a runtime setting or test recipe.

`crow setup` checks local rootless Podman and offers to install missing packages on Ubuntu/Debian as part of normal onboarding. System package installation uses sudo with the operator's consent. Crow configures cgroup delegation in its own systemd service and checks runtime prerequisites before completing setup. It builds its testing image automatically on first use.

Hosts still need user namespaces, seccomp and cgroup v2. Crow cannot provide missing kernel capabilities inside a restricted hosting environment. Setup shows the actual failure and lets the operator fix the host or continue with runtime testing disabled. It does not silently change kernel security settings or allocate subordinate user IDs. On other distributions, setup explains which packages or equivalent tools are needed. Crow itself always runs as the normal user, not root.

Existing installations without a runtime setting also receive the default when upgraded. Explicit disable settings and repository allowlists are preserved. Rerun `crow setup` after upgrading to install missing dependencies and regenerate the service configuration. To disable runtime testing:

```sh
crow config worker.execution '{"automatic":false}'
crow service-restart
```

To re-enable it, set `worker.execution` to `{"automatic":true}` and rerun `crow setup`. `crow doctor --runtime` checks the runtime independently.

No image IDs, test commands, or new budget settings are required. Crow provisions a shared toolchain image itself. It currently contains Node/npm, Python/pip, Rust/Cargo, Go, C/C++ build tools, Chromium/Playwright, SQLite and PostgreSQL tools. The image is based on Alpine 3.23, whose stock packages provide Rust 1.91 and Node 24 while retaining Python 3.12. Package patch versions can change when the image is rebuilt. Projects requiring other versions or platforms may still need a custom image. Repository Dockerfiles are evidence for setup, never instructions to execute on the host.

First-use toolchain provisioning uses Podman's normal build network to fetch Crow's fixed Alpine and npm inputs. It does not use the project download gateway or include repository files. Operators requiring offline provisioning must preload an image and configure its immutable ID below. The restricted gateway applies to project dependency setup; tests run offline.

To restrict runtime testing to selected repositories with automatic images:

```sh
crow config worker.execution '{"automatic":false,"repositories":{"owner/repo":{}}}'
```

An entry without `image` uses `"image":"auto"`. Existing entries specifying a local immutable `sha256:` image ID still work. Those images must include the tools the project needs; dependency preparation through the package gateway requires Python 3, GNU tar, and `/opt/crow/proxy.py` from Crow's runtime image. `podman` can select a local executable path. These are local worker settings; a PR or connection-service job cannot grant execution authority.

## What Crow does

The reviewer discovers manifests and relevant CI/documentation, then selects setup commands. Discovery offers candidates for Node, Python, Rust and Go projects, including nested projects. These candidates are not promises that a particular command is correct. The reviewer inspects the project and adapts them. Discovery prioritizes root and shallow manifests, reports omitted projects in very large repositories, and honors exact npm/pnpm/Yarn versions declared by `packageManager`, including modern Yarn.

Node workspace members inherit their root's package manager and setup command. Discovery returns `setupDirectory` for installation and `directory` for the project's test/start commands. Nested package-manager pins or lockfiles beneath an ancestor workspace require inspection, even if a pin repeats the root version. Discovery warns and leaves commands empty instead of assuming the nested project is independent. Standalone pinned or locked projects retain their declared setup. Literal workspace paths, `*` and `**` are supported. Nested Yarn workspaces with an exact Yarn 2+ pin resolve to the ultimate installation root. Other nested workspace managers, unsupported patterns and pnpm workspace YAML require inspection; discovery leaves those command candidates empty instead of guessing npm.

When a manifest has an exact npm, pnpm or Yarn pin, discovery also returns `packageManagerBootstrap.install` and `.runner`. These install only that CLI under the writable `/workspace/.crow-tools` directory. For temporary isolated probes, invoking a prepared executable directly can avoid package-manager wrappers that try to reinstall the entire workspace during an offline test. The bootstrap remains available even when Crow cannot resolve workspace setup. It does not choose workspace dependencies or override the inspection warning. Discovery lists the managed image's available tools and writable paths; pnpm, Yarn, Corepack and project runners such as `vp` are not preinstalled.

Rust discovery includes `rust-toolchain` and `rust-toolchain.toml` files from the project and its ancestors. The stock Alpine `rustc` and `cargo` binaries do not enforce those files' version pins. The reviewer must check the installed compiler against the manifest's minimum Rust version and any exact toolchain requirement. Testing with a compatible stock compiler does not verify the exact pinned compiler. If an exact version is unavailable, an operator-provided image containing it is required; Crow does not automatically download replacement Rust toolchains.

`prepare_environment` runs installation in a fresh sandbox at a pinned revision. It saves the resulting workspace only after successful preparation. Discovered Node package-manager installations are separate for each manager/version, and Python virtual environments are separate for each project directory. Preparing one project does not replace another project's tools or Python dependencies. Dependencies and caches must stay under `/workspace`; `HOME` is `/workspace/.crow-home`. Environment variables exported in one setup shell do not persist into later test shells, so test commands must activate a virtual environment or use explicit tool paths when needed.

Preparation can download packages through a restricted HTTPS gateway. The container still has no external network interface. A private Unix socket connects its loopback proxy to Crow's gateway, which accepts only selected public package hosts on port 443. Before connecting upstream, it requires a valid TLS ClientHello whose server name matches the requested package host. Missing names, mismatches and encrypted ClientHello extensions are rejected. It then rejects private and special IP addresses after DNS resolution and connects to the validated address. It does not forward worker credentials. Direct connections to unlisted hosts, private registries and production services are unavailable.

Crow checks the container's connection to that socket before running setup and reports access failures at the package gateway startup stage. The private socket mount requests a per-container SELinux label (`:ro,Z`); Crow does not disable container labeling. On SELinux-enforcing hosts, policy must also permit the container to connect to the worker's Unix socket. Relabeling the directory alone may not permit this. Enforcing SELinux configurations have not been validated, so dependency preparation on those hosts is not yet claimed as supported. Offline experiments do not mount or use this socket.

The package hosts currently cover npm/Yarn, PyPI, crates.io, the Go module proxy and checksum service, Maven Central and RubyGems. Inclusion of a package host does not mean every language toolchain is installed. Requests, concurrent connections and connection lifetimes are bounded. The gateway allows 16 active tunnels. When another connection is waiting, it releases established tunnels with no traffic in either direction for 15 seconds, so idle keep-alive connections cannot block new downloads indefinitely. A slow response that stays silent that long under full capacity can also be interrupted and retried by the package manager. Connections without waiting demand retain the 120-second lifetime limit. The gateway carries end-to-end HTTPS tunnels. It checks the CONNECT destination, resolved IP addresses and initial TLS server name, but cannot inspect encrypted HTTP Host headers, request paths or uploads. An allowed service that supports HTTP domain fronting may still route requests to another tenant. This is a restricted dependency network, not a package vulnerability scanner or a guarantee against data uploads. ClientHello inspection is limited to 64 KiB, 16 TLS records and five seconds; clients that require encrypted ClientHello are unsupported.

`run_experiment` restores the selected prepared workspace into a fresh, offline container. It verifies that the environment belongs to the exact requested commit and current image, then restores tracked source from that commit over the dependency snapshot. Setup hooks cannot silently replace the tracked source under test. The download socket is absent. Crow can run existing tests, write temporary reproductions, start services and exercise them through loopback. It compares equivalent experiments on base and head before attributing failures to the change.

Each experiment starts from its selected prepared snapshot. Files and build outputs created by one `run_experiment` are discarded before the next; logs and requested screenshots are retained separately. Put reusable build outputs in `prepare_environment` alongside dependencies, or build and test in the same experiment. Prepared snapshots are capped at 512 MiB even when the writable workspace limit is larger. If compiled output exceeds that cap, save dependencies alone and build during the test.

Identical preparations can be reused within the same review, at the same commit and image. These full workspace snapshots are not shared across reviews. Their tracked source is restored from Git before each test. Crow never extracts repository or prepared-workspace archives on the host.

## Dependency reuse and cleanup

Every new review, including one triggered by a PR update, installs dependencies in a fresh workspace. Package downloads, installed dependencies and build outputs are not shared across reviews. This prevents one review's writable package caches from changing what a later review installs. Crow handles installation automatically; users do not need to clear caches or provide new settings. Successful prepared environments remain reusable within the same review, so repeated experiments do not reinstall dependencies.

The shared toolchain image remains reusable across reviews.

Containers, services, browsers and temporary workspaces are removed after each experiment. After Crow saves and submits a valid report, it releases that review's prepared workspace snapshots. Completed, superseded and cancelled jobs also release snapshots during maintenance. Paused and active reviews retain them for resumption. Receipts, logs and screenshots follow the normal configurable seven-day review retention.

Maintenance runs at worker startup, hourly, and after reviews finish. It retries stopped-container cleanup, removes owned temporary files left by crashes, and prunes old Crow-managed image tags after seven unused days. Current images, images needed by resumable reviews, and explicitly configured image IDs remain protected. Cleanup touches only resources recorded by this Crow installation; it never runs a global Podman prune. Unregistered images from older Crow versions are left alone because ownership cannot be proved. PR closure follows the normal cancellation/terminal cleanup path, so storage cleanup does not depend on a PR eventually merging.

After the provider exits, Crow independently retries removal of that review's receipt-owned containers. This also covers a provider killing its MCP helper before removal finishes. The parent waits for the execution lock and preserves paused snapshots. `runtime-provider-cleanup.json` records the outcome; failures to write these diagnostics do not replace the original review result.

When cleanup fails, Crow retains the ownership receipts needed to retry instead of forgetting the leftover resources. `crow status` flags maintenance warnings; `crow status --format json` includes the details. `runtime-maintenance.json` in the data directory records the last cleanup result, and `crow cleanup` runs maintenance explicitly.

## Status in the main comment

Runtime tests are selective. Execution is available by default unless the worker operator disables or restricts it. The reviewer chooses useful checks based on the diff, project manifests, CI and available dependencies. Crow instructs the reviewer to investigate changed behavior autonomously, but does not require a test command on every review. Documentation-only changes can need no runtime check; unsupported platforms or private dependencies can prevent one.

The main Crow status comment shows runtime testing separately from review progress. It reports disabled execution, a pending testing decision, environment setup, running tests, and final command counts. Setup attempts have their own counts. If the reviewer finishes without running a test command, the comment says so. If setup fails before any tests run, it reports that testing was blocked. Interrupted jobs never leave a command labelled as still running. MCP cancellation targets the active request, stops its runtime command and waits for cleanup; disconnecting the client also cancels active setup or tests.

Counts come from saved execution receipts, not the model's description of its work. Workers send updates on their ten-second heartbeat; very short experiments may appear only as completed counts. The service updates the existing comment when those counts change. Counts include both base and head commands and unsuccessful investigation attempts, so a failed command does not itself mean the PR introduced a bug. The review explains what Crow checked, what it observed, and what it could not verify. Runtime commands have short purpose labels, so the report can say which behavior was checked on the PR version and before the PR. Setup and test outcomes are counted separately. Commands, commit IDs, exit codes and failure stages appear in an expandable diagnostics section instead of a wide table. These counts describe command attempts, not individual assertions or confirmed bugs.

For example, the main comment can report:

> **Runtime testing:** Finished. Test commands: 2 passed, 1 failed. Setup attempts: 2 passed. Failed commands can include base failures and investigation attempts; see the review for confirmed bugs.

The review explains the result in plain language, such as "The checkout smoke test passed before and after the change, but the screenshots show that the new graphic hides the payment label." A compact count stays visible; individual attempt outcomes and command diagnostics are collapsed underneath. A passed screenshot command can still reveal a visual bug.

## Visual investigations

The managed image supplies Playwright at `/opt/browser/node_modules/playwright-core/index.mjs` and Chromium at `/usr/bin/chromium`. Launch Chromium with `--no-sandbox --disable-dev-shm-usage` inside Crow's constrained outer container. The reviewer can navigate the running application, interact with it and capture screenshots using ordinary Playwright commands.

An experiment can request up to three PNG artifacts by absolute container path. Crow exports and validates each image before removing the container. Each image must be under 4 MiB and no larger than 4096 pixels in either dimension. `read_artifact` returns actual MCP image content, with the generating command and commit, so a vision-capable reviewer can inspect it. DOM and accessibility checks alone do not establish visual correctness.

Artifacts and receipts remain in the worker's review directory and follow normal review retention. They are not automatically uploaded to GitHub. A missing or invalid artifact is reported explicitly without converting a successful test command into a claim of visual verification.

## Limits and failures

Existing experiment limits still apply. Defaults are 120 seconds per setup/test command, 1 GiB of RAM, 512 MiB for each writable filesystem, two CPUs, 128 PIDs, and 12 attempts per review. Existing overrides still work; no new budget configuration is needed. First-time toolchain provisioning has a separate internal 30-minute ceiling and remains subject to review cancellation and the existing review timeout. It is recorded separately as `provisionMs` so a cold image build does not consume the command's runtime deadline.

Execution receipts record the current stage and where failures occurred, such as runtime availability, image preparation, source restoration, setup commands, test commands, exports, or cleanup. The main comment and the review's expandable diagnostics show stage names and warnings. Detailed bounded errors and command logs stay in `reviews/<job-id>/experiments/<experiment-id>.json` on the worker; they are not copied into public status comments. Setup receipts also retain a bounded list of package-gateway errors, including TLS rejections that package clients may describe only as connection failures. Successful commands remain successful if screenshot collection or cleanup fails, with those failures reported separately.

Captured output retains a bounded beginning and end with an omission marker, so lengthy logs retain their final diagnostics.

Every attempted setup or experiment counts toward the existing attempt limit, including prepared-environment reuse and failures. Discovery and reading saved evidence do not. The main reviewer executes serially; delegated reviewers remain inspection-only. Resumes retain receipts and successful preparations.

Reports distinguish setup from test commands. Passing dependency installation does not mean tests passed. An environment failure, timeout, pre-existing test failure or missing screenshot must not be reported as proof of a PR regression. If Crow cannot repair the setup, it continues inspection and explains the concrete gap in runtime coverage.

Source and dependency code run only in rootless containers with no host workspace mounts, host credentials, Linux capabilities or privilege escalation. The root filesystem is read-only. During setup, the sole host mount contains the restricted download socket; tests have no host mounts. Memory, CPU, process count, scratch space, output and time are bounded. Crow verifies resource limits before extracting source. Cancellation stops the container; interrupted work is recovered on resume. Rootless containers share the host kernel, so workers reviewing hostile code should run on dedicated machines or VMs.

The image includes PostgreSQL client/server tools, but its current container user and dropped privileges prevent ordinary `initdb` server initialization. Their presence does not imply PostgreSQL-backed applications can be started.

Linux workloads are the first backend. Native macOS/Windows applications, GPUs, devices, private dependencies and authenticated external services can block runtime investigation. Source archives do not materialize Git metadata, submodules or LFS objects, and are limited to the configured workspace size, capped at 512 MiB. Crow reports those limits rather than treating untested behavior as verified.

To disable execution, replace the worker setting with `{"automatic":false,"repositories":{}}` and restart the service.
