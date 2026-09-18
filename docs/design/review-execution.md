# Review execution decisions

These decisions were confirmed during the design interview. They describe intended behavior, not the current implementation or a complete implementation plan.

Each worker allows three concurrent PR reviews by default, configurable by the operator. Within each review, the reviewer may delegate analysis to parallel subagents. The initial limit is eight simultaneous subagents per review, also configurable. Eight is a ceiling, not a target: the reviewer should delegate only as much work as is useful. The reviewer combines the results into one report.

## Worker assignment

The first release assigns one worker per enrolled repository, with each worker able to serve several repositories. Distributing different PR reviews from one repository across multiple authorized machines is a possible future extension, not part of the current implementation scope.

A repository's eligibility configuration and the worker assigned to an individual job remain distinct to preserve the option of future scheduling by available capacity. Exactly one worker may actively own a given review job; multiple worker connections must not create duplicate reviews. No multiple-worker scheduler is required for the first release.

Paused reviews resume on the original worker using its local provider session. Session transfer between machines is outside scope and is not required for a future scheduler that assigns new reviews across workers. If the original worker is unavailable, continuing elsewhere means an explicitly requested fresh review with exclusive ownership established first. Automatic cross-machine failover is outside the current scope.

## Review execution

The Codex integration uses `codex exec` with subscription authentication. Crow supplies inspection tools and manages delegated child sessions to enforce model and execution policies. Installed-runtime probes verify parent/child interruption and saved-session continuation with local synthetic responses; live subscription refresh still needs validation. See ADR 0008 and `docs/design/runtime-validation.md`.

A newer PR revision supersedes an unfinished review of an older revision. Crow cancels obsolete work and queues the latest revision. Reports identify the commit actually reviewed.

Reports contain inline findings and a short summary written primarily for coding agents. Findings explain the problem, triggering conditions, consequences, and supporting code evidence. A successful review without actionable findings says so explicitly.

## Model configuration

The operator can configure the review model and reasoning level through Crow's terminal configuration. The choices must be supported by the selected provider and subscription account. Crow must report unavailable or incompatible settings rather than silently substitute another model or reasoning level.

During initial setup, Crow selects and displays the authenticated provider's reported default model and reasoning level, with an opportunity to change them. Crow stores explicit selections afterward. A later change to provider defaults does not silently change the installation's configured model or reasoning level.

Worker-wide model and reasoning defaults support per-repository overrides. Subagent settings have exactly two modes: Inherit, the default, uses the main reviewer's model and reasoning level; Configured uses the operator's separate subagent model and reasoning settings. The reviewer does not independently choose or override these settings. Crow must verify enforcement, account for provider custom agent configurations that can override spawn settings, and record actual selections.

Crow obtains available model choices and their supported reasoning levels from the provider integration rather than maintaining a model catalog in this repository. Discovery must use the subscription-authenticated provider and must not require a separately billed API key. For Codex, the documented app-server `model/list` interface supplies model identifiers, display names, defaults, and supported reasoning efforts. It can serve discovery while `codex exec` continues to run reviews. The CLI also documents an experimental `debug models` command, whose raw JSON schema is not established as stable. The returned catalog reflects the authenticated runtime's view, not a guarantee of fresh entitlement validation.

If refreshing the catalog fails, Crow explicitly notifies the operator that the current list could not be retrieved and displays the last successfully retrieved list, marked as cached. Existing reviews may continue with their configured models. Without a cached list, setup reports discovery as unavailable and offers a retry. Crow must not invent model choices or silently substitute a model.

Crow captures model and reasoning settings when a review starts and preserves them during automatic recovery. Changes to defaults apply to new reviews. The operator may explicitly resume a paused review with different settings while retaining saved context where the provider supports it. Exact Codex exec behavior requires integration verification. Record effective settings and intentional changes with the review metadata.

## Enrollment and recovery

Enrollment leaves existing PRs alone by default. The operator can explicitly include existing PRs. Later qualifying events or manual requests can make an initially excluded PR eligible. Crow preserves the enrollment decision across restarts so catch-up does not import the initial backlog unintentionally.

Startup and recovery catch-up are enabled by default and can be disabled. Repeated reconnects must not trigger overlapping scans. Large recovery batches are held for operator action, with a configurable threshold; the proposed starting threshold is ten reviews. Fresh webhook jobs can continue to receive priority. Restarting must not bypass a held batch. Disabling catch-up does not discard jobs already received through webhooks.

## Revision tracking and findings

A completed review identifies its repository, PR, reviewed head commit, target branch, and comparison base. Advancing the head or changing the comparison requires another review. An unrelated target-branch commit that leaves the comparison unchanged does not alone require a rerun.

Reports visibly identify the reviewed commit, target branch, and comparison base, with full revision identifiers in machine-readable completion metadata. Crow recognizes completion using its own authenticated published review records. Failed or cancelled attempts are not completed reviews; successful clean reviews are.

Later summaries represent earlier findings through links rather than posting identical inline comments again. Omission from a new review must not imply that an earlier finding was fixed. A summary may identify a finding as still present or fixed when it has actually been reassessed; otherwise it must describe its current status as unverified. GitHub marking a comment outdated is not evidence of a fix.

Keep this presentation brief. A label such as "Earlier findings not reassessed" with links is sufficient; repeated explanatory disclaimers are unnecessary.

This is a reporting requirement, not a requirement to supply all earlier findings to the model on every review. Crow preserves earlier GitHub comments and represents them in subsequent summaries so an agent reading the latest report does not mistake silence for resolution. Missing or unavailable history must not be silently interpreted as resolved findings. The mechanism for tracking and reconciling finding history remains to be designed.

## Review guidance

Crow accounts for applicable AGENTS.md project guidance during code review. Crow-specific review rules control review emphasis and reporting. Repository guidance cannot override operator controls, including author authorization and the configured execution boundary. Optional runtime experiments use isolated containers under ADR 0009; guidance cannot enable them.

Repositories can supply optional custom review rules in `.crow/review.md`. This file is absent by default; Crow works without it. Its purpose and optional nature must be explained in user documentation. The file should explicitly identify its instructions as applying to Crow PR reviews, while shared engineering conventions remain in AGENTS.md. This keeps review policy versioned with the repository without presenting it as general implementation-agent guidance.

Crow reads both review rules and applicable AGENTS.md guidance from a pinned target-branch revision selected when the review starts. Proposed instruction changes in a PR do not govern that PR's own review; they apply to later reviews after merging. The selected guidance applies to the main reviewer and its subagents.

Rule changes apply when the next review starts and do not automatically queue existing PRs again. An explicit review request can apply updated rules to an unchanged PR. Crow records a fingerprint of the effective guidance with each review so it can identify reviews produced under older instructions. Code-comparison freshness and instruction-version freshness are distinct; an instruction-only change does not create an automatic catch-up job.

## Manual requests and replies

The operator can request a review through the terminal. On GitHub, `/crow review` requests another review, including for an unchanged commit. GitHub requests are limited to the operator and explicitly authorized accounts. The PR must still satisfy the enrolled repository's author policy and draft restrictions. Requests for a revision already queued or running do not create duplicate jobs.

When usable incomplete session state exists for the same PR comparison, an ordinary review request resumes it. If the comparison has already received a completed review, the request starts a fresh review. A separate explicit restart command begins the analysis again instead of continuing the saved session.

Ordinary replies to Crow findings do not trigger model work in the first version. Users and agents can discuss findings, push corrections, or explicitly request another review. Conversational responses and commands to reconsider individual findings are outside the initial scope.

## Interruption and recovery

The proposed fixed 30-minute cutoff was rejected because it could discard productive work near completion. The current direction is no hard elapsed-time limit by default, with visible activity, explicit operator control, and an optional operator-configured limit.

Crow must support continuing an interrupted review from saved provider context where the provider supports it, including recovery after an error prematurely ends model work. A manual pause preserves state and releases the concurrency slot after the review's active work has stopped. Before resuming, Crow checks that the PR remains eligible and its comparison is unchanged. A newer revision supersedes the old review. Persisting event logs alone must not be described as lossless recovery of unfinished model work.

Recoverable provider errors continue the saved session. Invalid final output can be corrected through that session instead of repeating the investigation. Authentication and quota failures retain the session until recovery. A valid completed report is saved before publication so a GitHub failure requires a publication retry only. If no usable session survives an interruption, Crow reports that a restart is required rather than silently starting the investigation again.

The first version publishes findings only after the main reviewer finishes consolidating and validating a complete report. Interrupted analysis remains saved and GitHub shows the review as incomplete. Preliminary subagent findings are not published as a partial review.

The default transient-failure recovery strategy is fixed interval: ten consecutive retries with a nominal five-second delay after each failure. Both the interval and retry count are configurable. An optional progressive strategy retries after 5, 15, and 30 seconds, followed by 1, 2, and 5 minutes. Normal setup uses the fixed-interval default without requiring a retry-strategy decision.

Both strategies count retries after the initial attempt and measure each delay from the preceding failure, not from the start of the review. Attempts for the same review must not overlap. Small random staggering and longer provider-requested waits can extend the nominal interval. Exhausting the retry allowance leaves the saved review paused. A separate interruption after confirmed successful progress can receive a fresh allowance; simply restarting a process or reconnecting does not establish successful progress. Waiting reviews release their active slot after their processes stop. Common provider outages share a cooldown and delay new starts. Semantic output errors, authentication failures, and exhausted subscription quota need their own recovery policies rather than blindly using the transient-outage schedule.

Official Codex documentation supports continuing an explicitly identified saved session with `exec resume`. Resumable reviews cannot use the prototype's `--ephemeral` option. Crow must validate resumed output; schema behavior on resume, graceful interruption, subagent cleanup, and concurrent native subscription-authentication refresh require verification with the chosen runtime. Copied authentication caches are not an established solution to concurrent refresh.

## Local data retention

Crow retains paused review sessions while the PR remains open and the saved comparison remains relevant. Completed, superseded, and closed-PR session data is eligible for deletion after seven days by default, configurable. Diagnostic logs have the same default retention period. Compact records of reviewed revisions and published findings remain while the repository is enrolled. Local cleanup does not delete GitHub comments. An explicit cleanup command is available to the operator.

## Operational visibility

Review status is visible both on GitHub and on the worker's host. The first version uses one updatable status comment per PR, showing the relevant commit, current state, last-update time, and any action needed. States include reviewing, retrying, paused, and completed; completed status links to the published review. An incomplete review must not appear to be a successful review with no findings.

GitHub receives concise operational explanations. Detailed errors and diagnostic logs remain on the host. The connection service maintains the status comment so it can report worker disconnections even when the worker cannot update GitHub itself. Native GitHub checks are outside the initial scope; status comments preserve the advisory design without becoming required checks.

## Installation and updates

Crow reuses the operator's existing official Codex installation. If Codex is absent, setup may install the latest official release from OpenAI. A special Crow-managed Codex version is not desired. Reusing the installation must not inadvertently apply unrelated global AGENTS.md or other personal agent configuration to a review. Crow supplies explicit invocation settings and trusted target-branch instructions. Isolation must be validated for primary, subagent, and resumed sessions. Runtime probes showed that invocation settings alone do not suppress personal global instructions. Crow therefore uses a separate settings/session directory with one subscription login while continuing to use the same installed executable. This login does not replace the operator's usual Codex login.

Crow updates are explicit through `crow update`, with notifications when an update is available. Updating stops accepting new work, lets active reviews finish, and then updates and restarts the service. Crow must not silently replace an existing user-managed Codex installation.

## Connection-service storage and hosting

The connection service retains account and worker associations, author policy, durable jobs, completion tracking, and GitHub status identifiers. Checkouts, provider authentication, saved review sessions, and detailed model output stay on workers. Webhook bodies can contain private PR descriptions and comments; the service extracts and durably stores the information needed for processing, then discards raw payloads. Short operational logs use a seven-day default retention period.

Each user hosts their own Crow installation, including a connection service, durable storage, worker, and GitHub App. The operator's own installation runs on gibo. Separate native services preserve their roles without requiring rented compute or Docker. Other installations do not depend on gibo. The earlier shared-service default is superseded by ADR 0007.

One guided Crow setup flow supports both components together, connection-service-only setup, and worker-only setup. It configures the selected services, storage, persistent startup, and public HTTPS route wherever automation is available. The operator should not have to deploy the service separately or manually assemble its networking for the recommended path. Necessary provider account sign-ins and GitHub ownership/installation choices can occur through browser steps within that flow. Setup must be well documented, including headless use and deployments with components on separate machines.

Guided Tailscale Funnel is the default for independent setup and is included in the first release, with Cloudflare Tunnel and configurable HTTPS as alternatives. The previously selected `connect.birdapp.dev` Cloudflare endpoint can serve the operator's own installation; it is not a dependency for other users. Crow's connection service accepts a public HTTPS base URL without depending on a particular ingress provider. See `docs/design/setup-and-deployment.md`.

For each installation, Crow uses GitHub's App manifest flow to prefill registration and receive generated credentials automatically after browser confirmation. App creation and repository selection remain visible owner choices; no manual copying of App keys or webhook secrets is required by that flow. A worker-only setup pairs with that operator's existing service and does not create another App or public endpoint.

If a machine hosting both components goes offline, its installation cannot update GitHub status until recovery. Independently hosted installations remain unaffected. If only the worker process fails while the connection service remains online, the service can still report that failure. Status timestamps and recovery reconciliation remain important.

Failed-webhook deliveries are audited once per hour by default, configurable, with one audit shared across the GitHub App rather than one per repository. Normal webhook handling remains immediate. Audit recent deliveries after service recovery as well, and retain manual catch-up.
