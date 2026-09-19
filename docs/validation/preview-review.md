# Crow reviews its runtime feature

On 19 September 2026, PR12 was marked ready and reviewed by both the existing inspection-only Crow and a separate installation built from commit `7592cfd5cf0b173724820f1c6e1a368f02ccfbe6` with runtime testing enabled.

The preview uses separate service state, provider sessions, Podman storage and a loopback port. A deployment-only patch gives its GitHub status and review markers a distinct namespace and visible "Crow runtime preview" label. It does not change the existing App webhook. A PR12-only watcher requests reviews for new head/base combinations and stops the preview after GitHub confirms merge. These deployment files are outside the repository.

## Observed first review

The [preview review](https://github.com/Byntham/crow/pull/12#pullrequestreview-5254959399) performed seven experiments without a supplied test recipe:

- Fetched Rust dependencies at head and base.
- Attempted the complete Rust suite at both revisions. Both failed because the managed image has Rust 1.87 and Crow requires 1.88 or newer. An attempted toolchain download was rejected by the package gateway. The report identified this coverage gap rather than attributing it to the PR.
- Ran focused Python package-cache and source-restoration probes successfully.
- Generated the checkout fixture, ran both Chromium smoke tests, saved both screenshots and inspected them through two successful native-image `read_artifact` calls.

The seven receipts recorded four successful commands and three failed commands, counting setup separately from tests. After report acceptance, the preview removed all prepared snapshots and retained the receipts and two screenshots. Cleanup recorded no warnings. The inspection-only review and preview each kept their own status comment.

These observations verify execution, failure diagnosis, before/after investigation, image delivery, publication and completed-review cleanup. They do not establish that Crow's full Rust suite ran inside the managed image, or that the reviewers' findings were discovered dynamically. Both reviewers labelled their findings as inspection-based.

## Findings addressed

The [existing Crow review](https://github.com/Byntham/crow/pull/12#pullrequestreview-5254948111) identified mismatched TLS names through CONNECT tunnels, Go ZIP directory checksum handling, partial exports surviving maintenance, and nested scripts losing the selected npm version. The preview identified dependency-socket access under SELinux and runtime receipt appendices exceeding whole-report size limits.

The follow-up adds bounded TLS ClientHello/SNI validation, hashes Go directory entries against independently verified Go checksums, removes partial exports from validated nested directories, and preserves the pinned package manager in nested scripts. Report appendices must fit whole-report validation; a full valid model report takes precedence over optional appended rows.

SELinux support remains conditional on host policy. Private socket relabeling and an explicit connection preflight improve compatibility and diagnostics without disabling container labeling. No enforcing SELinux host was available for validation; mock permission failures and ordinary rootless containers cover the implemented paths.

TLS stays end-to-end. The gateway cannot inspect encrypted HTTP Host headers or prevent uploads or domain fronting offered by an allowed endpoint. The runtime guide documents that boundary.

Generated screenshots and raw reports remain outside the current source tree. The historical visual evidence stays accessible through immutable commit links in the other validation notes.

## Follow-up verification

The corrected implementation passes 300 ordinary tests, including whole-report Unicode/byte limits, nested export cleanup, socket permission failures, exact Go directory-entry checksums, and bounded TLS parsing and diagnostics. Formatting and clippy with warnings denied also pass. All eight real-container tests passed. The Rust/Go scenario additionally passed with a fresh package-cache namespace, followed by verified reuse. The discovery scenario verified pinned npm 10.9.0, pnpm 9.15.4 and Yarn 4.9.2 during nested test/start commands, plus npm during an install lifecycle hook. A denied-host setup retained the exact gateway rejection reason in its receipt.

## Second review

The [next runtime preview](https://github.com/Byntham/crow/pull/12#pullrequestreview-5254990359), built from `dee35f6`, performed eleven experiments. It reproduced a Go cache bug by constructing a ZIP with a NUL in an entry name. Python truncated that name during verification, so Crow accepted a package that Go subsequently rejected with a checksum mismatch. The same probe at base confirmed that this cache path was introduced by the PR. Eight commands passed and three failed, including the repeated Rust version limitation. Both browser screenshots were inspected; completed-review cleanup again reported no warnings.

The cache verifier now rejects ambiguous ZIP names, including NUL truncation, legacy filename decoding and Unicode Path overrides. Tests cover both export and import and retain valid UTF-8 filenames as a positive control. Expected checksums were independently checked with Go.

The inspection-only reviewer also identified mixed-case repository policies losing their overrides, snapshots being removed when report submission was canceled, local replays ignoring termination signals, and historical replays reading guidance from the merge base instead of the target tip. Regression tests cover each corrected path. An ARM CI failure also exposed a freshly written mock executable racing parallel subprocess forks; the mock is now an immutable checked-in fixture.

The default managed image now uses Alpine 3.23, with Rust 1.91 and Node 24. The Rust/Go integration fixture requires Rust 1.88 and exercises edition-2024 let chains, so the obsolete compiler cannot silently return. Discovery exposes Rust toolchain pins and warns that stock compilers do not enforce them. Exact pinned versions remain distinct from minimum-version compatibility; replacement compiler downloads are not enabled.
