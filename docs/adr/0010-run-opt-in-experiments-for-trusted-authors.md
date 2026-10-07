---
status: accepted
amends: 0002
---

# Run opt-in experiments for trusted authors

Inspection alone cannot show whether a change breaks a program. Crow lets the main reviewer prepare dependencies and run focused commands, such as existing tests or a temporary reproduction, against the pinned head and comparison base. ADR 0002 still holds by default: nothing runs unless the worker operator enables it for a repository.

## Who can run code

Execution needs three things: the repository is listed in the worker's local `worker.execution.repositories`, the repository's author policy lists its authors explicitly (not `everyone`), and the PR comes from a branch of the repository itself, not a fork. The connection service reports the last two for each job and can only withhold execution; it cannot grant it. PR content and review guidance cannot change any of this. Delegated reviewers stay inspection-only.

Upgrades never enable execution. An installation without the setting behaves exactly as before.

## Sandbox

Experiments run in rootless Podman on the worker. Each attempt starts a fresh container from Crow's managed image, which the worker builds from a fixed Containerfile. Containers run as a non-root user, which rootless Podman maps to a subordinate host UID rather than the operator's own account. They have no capabilities, no privilege escalation, a read-only root filesystem, bounded tmpfs scratch space, and verified cgroup limits on memory, CPU and processes. Tests have no network and no host mounts.

Setup commands, which install dependencies, reach a fixed list of public package registries through a gateway in the worker. The gateway checks the requested host, the TLS server name and the resolved addresses, and bounds connections and traffic. TLS stays end to end, so the gateway cannot see paths or uploads; setup code can still send data to an allowed registry or bucket. A successful setup saves the workspace so later tests in the same review can reuse it; Crow restores the pinned source over it before each test.

Rootless containers share the host kernel. This boundary suits code from authors the operator already trusts. It is not meant for untrusted public contributions; those stay inspection-only.

## Evidence

Every attempt counts against a per-review budget and leaves a receipt with its command, purpose, commit, outcome and bounded output. Receipts survive resumes, and the published report includes a short summary of what ran. The reviewer treats results as untrusted evidence and compares base and head before attributing a failure to the PR.

## Not included

Browser screenshots, custom images, other operating systems and cross-review dependency caches are left for later decisions.
