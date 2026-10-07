# Run tests during reviews

Crow can let the reviewer install a project's dependencies and run focused commands, such as the tests for the changed code or a short reproduction, before and after a PR. This is off by default. Turn it on per repository, for code from authors you already trust.

## Who it applies to

A review can run code only when all of these hold:

- The repository is listed in this worker's `worker.execution` setting.
- The repository's author policy lists its authors (`crow policy owner/repo --authors ...`), not `--everyone`.
- The PR comes from a branch of the repository itself, not a fork.
- Listed authors made every push to that branch since it was created.

Anything else is reviewed by inspection only, as before. A PR, its review guidance and the connection service cannot turn execution on. Delegated subagents never run code.

Crow checks pushes with GitHub's record of who made them, not commit authors, which anyone can set. If you set `crow policy owner/repo --pushers anyone`, the last condition no longer applies, and anyone with write access to the repository, including bots, can get code run on this machine.

## Turn it on

The worker needs rootless Podman, with cgroup v2 and the cpu, memory and pids controllers delegated to your user. On Ubuntu or Debian:

```sh
sudo apt-get install podman uidmap
```

Then list the repositories, restart, and check:

```sh
crow config worker.execution '{"repositories":{"owner/repo":{}}}'
crow service-restart
crow doctor --runtime
```

The worker builds Crow's runtime image in the background the first time, using Podman's normal network. It contains Python, Node and npm, Rust and Cargo, Go, a C toolchain, Git and SQLite. Until it is ready, reviews continue by inspection. To turn execution off, remove the repository from the list, or set `worker.execution` to `{}`.

Each repository can override the defaults:

| Setting | Default | Meaning |
| --- | --- | --- |
| `timeoutSeconds` | 300 | Time per command |
| `memoryMiB` | 2048 | Memory per container, including its scratch space |
| `workspaceMiB` | 1024 | Size of `/workspace` |
| `cpus` | 2 | CPU limit |
| `pids` | 256 | Process limit |
| `maxRuns` | 12 | Attempts per review, including setup and failures |

For example, `{"repositories":{"owner/repo":{"timeoutSeconds":600,"maxRuns":20}}}`. Set `"podman"` beside `repositories` to use another Podman executable.

## What happens in a review

The reviewer decides whether running code would help. It prepares an environment for the PR version or the merge base with a setup command, such as `npm ci` or `pip install -e .`. Setup can download from public package registries (npm, PyPI, crates.io, the Go module proxy, Maven Central and RubyGems) through Crow's gateway; nothing else is reachable. Each gateway connection lasts at most `timeoutSeconds`, can download at most `workspaceMiB` and upload at most 16 MiB. A successful setup is saved, and later commands for that version start from it. Each command runs in a fresh container with no network, and files it creates are discarded afterwards.

The published report ends with a short list of what ran and how it went. Failed commands include the reviewer's own investigation attempts and checks that also fail before the PR, so a failure there is not by itself a finding. Command output stays on the worker in `reviews/<job>/experiments/`, which follows the normal retention period. Prepared environments are deleted when the review finishes.

## What the sandbox protects

Each container runs as a non-root user in a user namespace where your own UID is not mapped, so it cannot act as your account or read your files. Crow refuses to run experiments with rootful Podman. It has no capabilities, cannot gain privileges or create namespaces, has a read-only root filesystem and bounded scratch space, and its memory, CPU and process limits are checked before any repository file is unpacked. Tests have no network. Containers are removed after each command, and Podman stops them on its own if Crow exits.

It does not make hostile code safe. Containers share the host's kernel, so a kernel vulnerability could let code escape. Setup commands can send data to the registries they reach; that includes any bucket on `storage.googleapis.com`, which the Go module proxy uses. Only enable execution for repositories whose authors you would let run code on this machine.
