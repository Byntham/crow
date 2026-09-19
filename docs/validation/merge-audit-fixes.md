# Merge audit fixes and verification

This records the earlier audit at commit `21c2e11`. Its cross-review workspace cache has since been replaced by verified package-download reuse. See [the subsequent cleanup and diagnostics audit](runtime-hardening.md) for current behavior and validation.


The audit reproduced a source-restoration bypass, lost final log diagnostics, cache misses across production checkout paths, a misleading configuration display, and a CI image-ID incompatibility. The follow-up fixes those issues and adds runtime progress to Crow's main PR status comment.

| Issue | Change | Verification |
| --- | --- | --- |
| Repository `tarfile.py` could replace Python's standard library during pinned-source restoration | Run Crow's Python restoration, proxy and readiness helpers with `python3 -I`; invalidate older dependency snapshots | Real containers restore the original tracked content after setup tampers with it. Separate probes cover a tracked import collision and a writable user-site `.pth` hook that exits the interpreter. |
| Long output discarded final failure diagnostics | Retain bounded head and tail with an explicit omission marker | Chunk-boundary and invalid UTF-8 tests, plus a real failed command emitting 40,000 bytes before its final diagnostic. |
| Equivalent setups in separate review checkouts missed the cache | Key snapshots by normalized repository identity, exact commit, immutable image, setup command and cache format version | A second checkout reuses the first snapshot; the identical commit and command under a different repository identity do not. |
| Automatic execution displayed as disabled | Distinguish all repositories, selected repositories and disabled configuration | Configuration rendering tests cover all three. |
| Ubuntu Podman returned an unprefixed image ID and CI stopped before runtime tests | Normalize the inspected ID; run both real MCP and managed runtime suites sequentially in x64 CI | Local real-container suites pass. GitHub CI also exercises cold managed-image provisioning. |
| Main comment omitted runtime testing | Send receipt counts on heartbeat and final report; render setup and tests separately | Worker tests verify live updates without provider progress events. Service tests verify comment transitions, payload validation, stale lease rejection, final counters, and preservation when reusing a completed comparison. |

Local verification passed:

- 242 ordinary tests across all targets.
- Formatting and clippy with warnings denied.
- The real MCP regression/isolation/deadline/recovery/publication test.
- All four managed runtime scenarios: concurrent downloads; Rust/Go dependencies and offline tests; Node/Python setup recovery, cache, base/head regression and browser evidence; hostile Python imports, cross-checkout cache reuse and final diagnostics.

The MCP test fixture now allows 20 seconds for container startup and command completion. Its intentional 60-second command still verifies deadline enforcement. The previous eight-second fixture deadline was too short on the local VFS-backed container store. Production limits are unchanged.

[Runtime status examples](runtime-status.md) show the added comment text. Earlier [historical PR replays](historical-reviews.md) remain relevant; this follow-up did not rerun model-driven historical reviews. Tests of GitHub comment delivery use Crow's service and a simulated GitHub API, not comments posted to upstream projects.
