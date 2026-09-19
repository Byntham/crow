---
status: accepted
amends: 0009
---

# Prepare review environments automatically

Requiring project owners to maintain dependency images and explain each project's testing procedure limits the usefulness of runtime review. Crow will offer a worker-level automatic mode and automatic images for individual authorized repositories. Existing installations do not acquire execution permission implicitly.

The agent discovers setup from repository evidence, prepares dependencies in a sandbox, repairs setup failures, and runs focused offline experiments. Crow supplies a managed Linux toolchain and a restricted package-download gateway. The one host mount used during preparation contains only the gateway socket. Tests remain offline and receive no host mounts. Arbitrary repository Dockerfiles do not run in the trusted image builder.

A successful prepared workspace may be reused within the same review for the exact commit, setup command and immutable image. Across reviews, only verified package downloads may be reused; installed workspaces and build outputs are not shared. Full snapshots are released when a review finishes, while a bounded download cache expires idle entries independently of PR merge events. Prepared environments cannot be used across revisions or changes to the selected image. Tests and setup have distinct receipt phases. The existing review and experiment limits apply without new operator budget settings; cold toolchain provisioning has a bounded internal deadline.

Browser experiments may export bounded PNG artifacts. Crow validates and stores them, then returns native MCP image content when the reviewer requests an artifact. This replaces base64-in-text workarounds and enables actual visual inspection with a vision-capable provider. Automatic GitHub artifact hosting and other operating-system backends remain separate work.
