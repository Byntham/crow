---
status: accepted
supersedes: 0002
---

# Run isolated experiments under worker control

Inspection alone cannot establish whether a change breaks a program. Crow will let the main reviewer run focused tests, reproductions and application smoke tests against the pinned head and comparison base.

Rootless local Podman is the first backend. It supports many Linux toolchains without adding a hosted execution service or exposing worker credentials. Separate VMs would provide stronger isolation and support other operating systems, but require a scheduler, image lifecycle and additional infrastructure. GitHub Actions would reuse some projects' CI environments, but requires broader GitHub permissions and offers less direct interaction between the reviewer and an experiment. Neither is required for this first backend.

Local worker configuration enables automatic managed environments by default. Operators can disable execution, restrict repositories or select immutable local images and resource limits. Guided setup offers installation of missing Podman packages and checks host support. PR content, review guidance and connection-service jobs cannot enable execution or change its image. Existing installations without an execution setting adopt the default; explicit disable settings and repository allowlists remain unchanged. The default is resolved when loading local installation settings, never from missing execution authority in an internal reviewer context. Codex still has no host execution tools. Only Crow's `run_experiment` tool can execute repository code.

Each experiment runs in a fresh container with a source archive from one pinned commit. No host directory is mounted or unpacked into. The container has no external network, capabilities, privilege escalation or credentials. The image is read-only and scratch storage is bounded. Crow verifies cgroup resource limits inside the container before running source. Images contain trusted toolchains and dependencies prepared outside reviews. Rootless containers share the host kernel, so a dedicated VM remains appropriate for hostile multi-tenant workloads.

The main reviewer runs experiments serially. Subagents remain inspection-only. Durable receipts count all attempts against a per-review budget, retain results across resumes, and supply a deterministic table in the published report. The model receives results as untrusted evidence. It should compare the same reproduction at head and base before attributing a failure to the PR.

Cancellation removes the container, and a runtime deadline remains in force if Crow exits abruptly. Interrupted work is recorded as incomplete and cleaned up on the next tool call. There is no automatic retry of an experiment. The reviewer may make another attempt within the remaining budget.

See [configuration and limitations](../user/runtime-experiments.md) and [design](../design/runtime-experiments.md) and [validation](../maintainer/runtime-validation.md).
