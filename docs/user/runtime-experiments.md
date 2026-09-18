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

No image IDs, test commands, or new budget settings are required. Crow provisions a shared toolchain image itself. It currently contains Node/npm, Python/pip, Rust/Cargo, Go, C/C++ build tools, Chromium/Playwright, SQLite and PostgreSQL tools. These are stock Linux toolchains; projects requiring other versions or platforms may still need a custom image. Repository Dockerfiles are evidence for setup, never instructions to execute on the host.

Alternatively, enable selected repositories with automatic images:

```sh
crow config worker.execution '{"repositories":{"owner/repo":{}}}'
```

An entry without `image` uses `"image":"auto"`. Existing entries specifying a local immutable `sha256:` image ID still work. Those images must include the tools the project needs; dependency preparation through the package gateway requires Python 3, GNU tar, and `/opt/crow/proxy.py` from Crow's runtime image. `podman` can select a local executable path. These are local worker settings; a PR or connection-service job cannot grant execution authority.

## What Crow does

The reviewer discovers manifests and relevant CI/documentation, then selects setup commands. Discovery offers candidates for Node, Python, Rust and Go projects, including nested projects. These candidates are not promises that a particular command is correct. The reviewer inspects the project and adapts them.

`prepare_environment` runs installation in a fresh sandbox at a pinned revision. It saves the resulting workspace only after successful preparation. Dependencies and caches must stay under `/workspace`; `HOME` is `/workspace/.crow-home`. Environment variables exported in one setup shell do not persist into later test shells, so test commands must activate a virtual environment or use explicit tool paths when needed.

Preparation can download packages through a restricted HTTPS gateway. The container still has no external network interface. A private Unix socket connects its loopback proxy to Crow's gateway, which accepts only selected public package hosts on port 443. It rejects private and special IP addresses after DNS resolution and connects to the validated address. It does not forward worker credentials. Arbitrary URLs, private registries and production services are unavailable.

The package hosts currently cover npm/Yarn, PyPI, crates.io, the Go module proxy and checksum service, Maven Central and RubyGems. Inclusion of a package host does not mean every language toolchain is installed. Requests, concurrent connections and connection lifetimes are bounded. The gateway carries HTTPS tunnels; it is not a package vulnerability scanner or a guarantee against uploads to an allowed service.

`run_experiment` restores the selected prepared workspace into a fresh, offline container. It verifies that the environment belongs to the exact requested commit and current image, then restores tracked source from that commit over the dependency snapshot. Setup hooks cannot silently replace the tracked source under test. The download socket is absent. Crow can run existing tests, write temporary reproductions, start services and exercise them through loopback. It compares equivalent experiments on base and head before attributing failures to the change.

Successful preparations are reused when the repository location, exact commit, image and setup command match. A new commit always invalidates that snapshot. This also permits reuse across reviews when one PR's head becomes a later comparison base. Shared snapshots are pruned by age and an approximately 4 GiB size cap; each snapshot is limited to 512 MiB. Cache entries are archived with owner-write permission so unprivileged restoration can populate read-only module directories. Original tracked-file permissions are restored from Git before testing. Crow never extracts archives on the host. The cache is an optimization, not evidence that an application passed tests.

## Visual investigations

The managed image supplies Playwright at `/opt/browser/node_modules/playwright-core/index.mjs` and Chromium at `/usr/bin/chromium`. Launch Chromium with `--no-sandbox --disable-dev-shm-usage` inside Crow's constrained outer container. The reviewer can navigate the running application, interact with it and capture screenshots using ordinary Playwright commands.

An experiment can request up to three PNG artifacts by absolute container path. Crow exports and validates each image before removing the container. Each image must be under 4 MiB and no larger than 4096 pixels in either dimension. `read_artifact` returns actual MCP image content, with the generating command and commit, so a vision-capable reviewer can inspect it. DOM and accessibility checks alone do not establish visual correctness.

Artifacts and receipts remain in the worker's review directory and follow normal review retention. They are not automatically uploaded to GitHub. A missing or invalid artifact is reported explicitly without converting a successful test command into a claim of visual verification.

## Limits and failures

Existing experiment limits still apply. Defaults are 120 seconds per setup/test command, 1 GiB of RAM, 512 MiB for each writable filesystem, two CPUs, 128 PIDs, and 12 attempts per review. Existing overrides still work; no new budget configuration is needed. First-time toolchain provisioning has a separate internal 30-minute ceiling and remains subject to review cancellation and the existing review timeout. It is recorded separately as `provisionMs` so a cold image build does not consume the command's runtime deadline.

Every attempted setup or experiment counts toward the existing attempt limit, including cache hits and failures. Discovery and reading saved evidence do not. The main reviewer executes serially; delegated reviewers remain inspection-only. Resumes retain receipts and successful preparations.

Reports distinguish setup from test commands. Passing dependency installation does not mean tests passed. An environment failure, timeout, pre-existing test failure or missing screenshot must not be reported as proof of a PR regression. If Crow cannot repair the setup, it continues inspection and explains the concrete gap in runtime coverage.

Source and dependency code run only in rootless containers with no host workspace mounts, host credentials, Linux capabilities or privilege escalation. The root filesystem is read-only. During setup, the sole host mount contains the restricted download socket; tests have no host mounts. Memory, CPU, process count, scratch space, output and time are bounded. Crow verifies resource limits before extracting source. Cancellation stops the container; interrupted work is recovered on resume. Rootless containers share the host kernel, so workers reviewing hostile code should run on dedicated machines or VMs.

Linux workloads are the first backend. Native macOS/Windows applications, GPUs, devices, private dependencies and authenticated external services can block runtime investigation. Source archives do not materialize Git metadata, submodules or LFS objects, and are limited to 128 MiB. Crow reports those limits rather than treating untested behavior as verified.

To disable execution, replace the worker setting with `{"automatic":false,"repositories":{}}` and restart the service.
