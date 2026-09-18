# Run tests during reviews

Crow can run tests, reproduce a suspected bug, or start an application and probe it inside a disposable Linux container. Each experiment uses the exact PR head or comparison-base commit. The reviewer can run the same command against both revisions and use the results in its findings.

Execution is optional. Existing installations remain inspection-only until the worker operator enables a repository. Podman is required only on workers that run experiments.

## Prepare an environment

Install rootless Podman with an OCI runtime, user namespace mappings, seccomp, and delegated cgroup v2 CPU, memory and process controllers. On supported Ubuntu hosts, start with `sudo apt install podman uidmap`. Run Podman as the same unprivileged user that runs Crow. Check `podman info` and verify a resource-limited container runs under that user's systemd session. Crow rejects rootful and remote Podman.

Build or load a trusted image containing your project's toolchain and dependencies. It must provide `/bin/sh` and `tar`. Do this outside a review, using dependencies and build instructions you trust. Crow does not execute a PR's Dockerfile on the host or download packages during experiments.

For example, this image supports Python standard-library tests:

```sh
podman pull docker.io/library/python:3.13-slim
podman image inspect --format '{{.Id}}' docker.io/library/python:3.13-slim
```

Use the full `sha256:` image ID printed by the second command. A mutable image tag is not accepted. Private images work once loaded into that user's local image store. Do not bake credentials into an image. Crow preserves the image's environment so compiler paths and runtime settings work, but sets `HOME=/tmp` and `CI=true`.

For projects with dependencies, prepare an image from a trusted checkout. Examples include a Python virtual environment under `/opt/venv`, a Rust toolchain with a populated Cargo cache, or Node packages under `/opt/dependencies`. Keep these directories outside `/workspace` and `/tmp`, which Crow replaces with empty writable filesystems. Put offline setup instructions in `.crow/review.md`, such as copying cached Node dependencies into `/workspace/node_modules` or running Cargo with `--offline`. Images are read-only during experiments, so dependencies that need a writable cache must copy it into the workspace first.

An image can also include a database or headless browser. The experiment command can start services in the background, wait for readiness, and test them through loopback. All processes belong to the same isolated container and stop when the experiment ends. This version does not export screenshots or other binary artifacts; test commands return text output.

## Enable selected repositories

On the worker, replace the example image ID below with the ID from your local image store:

```sh
crow config worker.execution '{"repositories":{"owner/repo":{"image":"sha256:REPLACE_WITH_64_HEX_DIGITS","timeoutSeconds":120,"memoryMiB":1024,"workspaceMiB":512,"cpus":2,"pids":128,"maxRuns":12}}}'
crow doctor --runtime
crow service-restart
```

`worker.execution` is a local worker setting. It is not a service-side `repo-config` override. On split installations, configure the worker that owns the repository. Repository names must match the enrolled `owner/repo` name. To enable several repositories, add entries to `repositories`. Updating this object replaces the whole mapping.

Only `image` is required for each repository. The example shows all defaults. `timeoutSeconds` must be between 1 and 1800; `maxRuns` must be between 1 and 50. Every attempted experiment consumes one run, including environment failures. Running a comparison on both base and head consumes two runs. The budget survives review resumes; an explicit review restart starts a new budget.

The main reviewer runs at most one experiment at a time per review. Subagents continue to inspect code and can suggest experiments to the main reviewer. Worker review concurrency still applies, so size memory limits for all simultaneous reviews.

To disable all runtime experiments:

```sh
crow config worker.execution '{"repositories":{}}'
crow service-restart
```

The optional `podman` field selects an executable path, for example `{"podman":"/usr/bin/podman","repositories":{...}}`. It does not accept shell arguments. Crow leaves runtime installation and image preparation under operator control.

## What runs

The `run_experiment` tool accepts a shell command and either `head` or `base`. Crow supplies an archive of that pinned commit, without checking out source on the host. Repository export attributes do not hide or rewrite files. Executable permissions and symlinks are preserved. Extraction and all repository code run inside the container.

The command starts in `/workspace`. It can create a reproduction test, modify its disposable files, compile software, run existing tests, or launch an application. Every subsequent experiment starts fresh. Temporary tests must be included in the command to run them against both revisions.

Containers have no network route to the internet, host or other containers. Loopback services within the experiment work. They receive no host mounts, Docker socket, GitHub token or provider credentials. All Linux capabilities are dropped, privilege escalation is disabled, and the root filesystem is read-only. CPU, memory, process count, writable filesystem size and wall time are bounded. Crow verifies the container's actual cgroup limits before extracting source. Output is limited to 32 KiB per stream while Crow continues draining excess bytes. Container logs are disabled to avoid an unbounded copy on disk.

Rootless containers share the host kernel. Operators who accept hostile code from arbitrary authors should run the Crow worker on a dedicated disposable machine or VM as an additional boundary.

## Results and limitations

GitHub reports include outcome counts for every experiment and a table of up to twelve experiments with the commit, outcome, exit code and command excerpt. The worker retains the complete command, image ID, configured limits, elapsed time and bounded stdout/stderr in `reviews/JOB_ID/experiments/*.json` under `CROW_HOME`. The reviewer can retrieve these receipts with `list_experiments` after resuming. Normal review retention also removes these files.

Crow distinguishes successful and failed commands, environment errors, timeouts, and interruptions. A failed command alone is not evidence that the PR introduced a bug. The reviewer must consider the base result, source evidence and environment limitations. Exit codes 125–127 are treated as environment errors; a test suite using those codes may need interpretation. OOM kills and other signals can also reflect resource limits.

A pause or cancellation stops and removes the container. Podman's own deadline also stops it if Crow is killed abruptly. A resumed tool session removes an unfinished run before starting another experiment. Interrupted runs consume budget and never appear as successful results. An enabled review that runs no experiments says so in its report.

The initial backend supports Linux workloads that can run offline in one container. Native macOS/Windows apps, GPU or device access, external services and live credentials are outside its scope. Git metadata, submodule contents and Git LFS objects are not materialized; LFS pointer files remain pointer files. Source archives are limited to 128 MiB. Projects that require these features need a prepared fixture or a future backend.

`crow doctor --runtime` checks the runtime and configured images. It does not prove your application's dependency setup works. If a review reports a missing dependency, correct the image, update the configured image ID and restart the worker before requesting a fresh review.
