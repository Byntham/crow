# Crow implementation plan

Status: approved for implementation. The native CLI, service, worker, and automated validation are implemented; live account onboarding and a real GitHub review still require operator authentication.

The first release lets an operator install Crow on an always-on Linux machine, complete one guided terminal setup, and receive advisory reviews on eligible GitHub PRs using their Codex subscription. Every operator hosts their own complete installation. Native services run persistently without Docker.

The accepted behavior is detailed in [setup and deployment](setup-and-deployment.md), [review execution](review-execution.md), and [the independent-installation decision](../adr/0007-make-each-crow-installation-independent.md). Those documents supersede the earlier shared Crow service proposal.

## User experience

1. Install Crow and run `crow setup`. The default installs both the connection service and worker on this machine. Separate service-only and worker-only modes remain available.
2. Configure public HTTPS through guided Tailscale Funnel setup, or choose Cloudflare Tunnel or an existing HTTPS endpoint. Crow preserves other networking configuration.
3. Follow a browser link to register the installation's own GitHub App. Crow supplies the manifest and captures credentials automatically. The operator installs the App on selected repositories. Marketplace publication is unnecessary.
4. Reuse the official Codex executable already installed, or install the latest official release if absent. Keep Crow settings and subscription state in a separate directory, with one subscription login for the installation. Display the provider's device-login URL and code for completion on a separate desktop, then continue setup on the headless host. Document SSH callback forwarding as a fallback. Confirm the provider-reported model and reasoning defaults. Crow isolates review configuration from personal agent instructions.
5. Select repositories. Default to reviewing only the operator's PRs, skip drafts, and leave existing open PRs alone. Advanced settings remain optional.
6. Finish with connection and configuration checks and enable persistent startup. Setup neither offers nor runs a test review. Browser steps work from another device for a headless host.

The terminal provides configuration, status, logs, and lifecycle commands. Reviews and concise operational status appear on GitHub. Setup can resume after interruption without replacing completed configuration.

## Replaced prototype baseline

The previous Node application received webhooks, checked out source, called a provider CLI, stored jobs, and posted GitHub reviews. Setup required manual service, credential, and networking configuration. Its browser interface was a disconnected prototype.

The former `server/crow.mjs` combined service and worker responsibilities. `server/review-runner.mjs` used ephemeral Codex execution and a three-minute subprocess timeout. Comparison tracking used the target branch tip, publication deduplication happened after inference, and completion detection could accept unrelated bot identities.

The native implementation replaces that prototype. The sections below retain the approved implementation sequence and its validation requirements. See [runtime validation](runtime-validation.md) for completed checks and the remaining live-account validation.

## 1. Validate Codex compatibility first

Build a development probe using the official installed runtime before committing the execution design to production code. Review execution remains `codex exec`; app-server may supply the model catalog.

- Verify subscription authentication and concurrent credential refresh with three review processes. Avoid separate per-job copies of rotating authentication credentials.
- Verify isolation of global AGENTS.md, configuration, hooks, skills, and custom agents for initial execution, subagents, and resumed sessions. Runtime probes established that invocation controls alone are insufficient. Use a Crow settings/session directory with one subscription login and the same installed executable.
- Enforce file inspection, search, and Git diff access without allowing repository scripts, tests, dependency installation, writes, or pushes. Read-only filesystem access alone does not establish this boundary.
- Capture the explicit provider session ID, interrupt execution, and resume that session. Verify child shutdown, final-output validation, model overrides, and inheritance of configured subagent settings. Saved conversation history must not be described as a lossless checkpoint of interrupted work.
- Query provider model/default/effort information and exercise catalog refresh failure with and without a cache.

Completion requires recorded results and reproducible integration checks. If the official runtime cannot enforce a required behavior, document the specific limitation and resolve it before claiming support. These are development checks, separate from user onboarding.

## 2. Separate service, worker, and durable state

Keep one distributable CLI with separate native service roles. The connection service owns GitHub App credentials, webhook verification, repository policy, job dispatch, and GitHub status. Workers own source checkouts, subscription authentication, provider sessions, and analysis.

Use durable transactions for event receipts, enrollment exclusions, queue state, job ownership, validated reports awaiting publication, and published review identifiers. A verified webhook must be stored before acknowledgement. Discard raw webhook bodies after extracting required data. An embedded database is suitable for the default installation and avoids a separately administered database service.

Authenticate paired workers and scope their repository access. Assign one worker per repository. Reconcile disconnected workers without giving two processes ownership of the same review. Keep automatic multi-machine scheduling and session transfer outside this release.

Completion requires crash/restart checks showing that accepted events and completed reports survive without duplicate review execution or publication.

## 3. Implement eligibility, revision tracking, and scheduling

Handle qualifying PR events and explicit terminal or `/crow review` requests. Verify requester authorization independently of PR author policy. Include fork PRs targeting enrolled repositories, subject to the same policy. Skip drafts and supersede unfinished work when its comparison changes.

Identify comparisons by repository, PR, head SHA, target branch, and merge-base SHA. Use the PR's three-dot diff. Read applicable AGENTS.md and optional `.crow/review.md` from a pinned target revision and record the instruction fingerprint. Do not let PR instruction changes govern their own review.

Check completed comparisons before inference and recognize only this installation's authenticated App identity. Coalesce duplicate events and requests. Explicit requests can start another review of an already completed comparison.

Default to three concurrent PR reviews and at most eight simultaneous subagents per review, both configurable. Preserve initial-backlog exclusions across restarts. Enable startup/recovery catch-up, hold large batches for operator action, and allow manual catch-up. Use ten reviews as the proposed configurable hold threshold. Audit failed webhook deliveries hourly per App and after recovery; normal PR discovery remains event-triggered.

Completion requires checks for author policies, forks, drafts, duplicate deliveries, retargeting, force pushes, unchanged merge bases, restart exclusions, and held recovery batches.

## 4. Implement recovery and GitHub reporting

Persist session IDs and effective model settings. Support pause, resume, and explicit restart. Resume only when eligibility and comparison still match. If the saved session is unusable, report that a restart is required. There is no hard runtime limit by default.

Default to ten consecutive retries with a nominal five-second delay after each failure. Support the agreed progressive alternative and configurable retry settings. Respect provider waits, share cooldowns during common outages, and release slots once stopped processes no longer consume them. Authentication and quota failures pause work for resolution. Exhausting retries preserves the session. Reset consecutive failure counts only after actual successful progress.

Validate and store the complete report before publishing. Retry publication independently of inference, reconcile uncertain GitHub responses before reposting, and pin the actual reviewed commit. Recheck freshness before publication and handle a racing push without presenting an old review as current.

Publish advisory inline findings and a concise summary with visible comparison links and machine-readable completion metadata. Link earlier findings with a brief status such as "Not reassessed" where appropriate. Do not infer a fix from omission or an outdated thread, or require every new model review to ingest all previous findings.

Maintain one status comment per PR for reviewing, retrying, paused, and completed states. Keep detailed diagnostics local. Publish no partial findings after an interrupted review.

Completion requires recovery and publication fault tests, including cancellation with active subagents, malformed output, quota failure, lost responses, and a push during publication.

## 5. Deliver guided setup and persistent operation

Package the CLI for native Linux installation and manage persistent processes with systemd. Provide the combined default and both separate roles through one resumable setup flow. Automate storage paths and service configuration, and verify startup after reboot and without an interactive login.

Implement guided Funnel setup with a dedicated unused supported port and only Crow-owned bindings. Verify that public webhook access works without exposing existing private Serve routes. Include stable Cloudflare Tunnel configuration and existing-HTTPS support. Validate the Cloudflare authorization flow before documenting its exact automation steps.

Implement GitHub App manifest registration, credential capture, installation/repository selection, and worker pairing. Keep browser callbacks and administration appropriately authenticated. Verify actual GitHub access and provider login rather than checking whether configuration strings exist.

Model settings use worker defaults and repository overrides. Subagents support only Inherit and Configured modes. Show discovery failures and label cached catalogs. Store explicit choices after initial selection of provider defaults.

Completion requires a clean Ubuntu installation walkthrough, headless onboarding, interrupted setup recovery, a split-host walkthrough, and checks that pre-existing Codex and network configuration remain usable. Setup ends with diagnostics, without a test review.

## 6. Finish operations, documentation, and release validation

Provide terminal controls for status, logs, enrollment, configuration, review requests, pause/resume/restart, catch-up, cleanup, and service lifecycle. Implement explicit `crow update` with draining of active work. Never silently replace user-managed Codex.

Add optional manual encrypted export and restore for configuration, App credentials, and the service database. Reconcile restored state before starting jobs. Document that service backup does not transfer provider sessions. Defer scheduled backups.

Apply the agreed seven-day default cleanup to eligible sessions and diagnostics, retaining paused sessions while relevant and compact finding/revision records while enrolled. Never delete GitHub comments as part of cleanup.

Write the combined Linux quickstart, separate-role guides, Funnel and Cloudflare alternatives, headless authentication, review rules, all defaults and overrides, recovery, backup/restore, updates, and troubleshooting. Replace the prototype dashboard's role in onboarding with CLI documentation.

Run the repository's relevant automated checks plus the lifecycle and integration checks established above. Before release, exercise the complete event-to-review flow against a deliberately selected development PR, with the operator's authorization for provider usage and GitHub publication. This is release validation, not a setup feature. Document runtime compatibility limits with the results.

## Deferred scope

Claude Code, a substantial dashboard, native GitHub checks, automated repository edits, conversational bot replies, multiple workers scheduling reviews for one repository, session transfer, and scheduled backups remain outside the first release. A central Crow service and Marketplace publication are not required.

The prototype application has been replaced by the native implementation. Development validation uses local fixtures and the official Codex runtime where possible. No live service installation, networking changes, or GitHub review publication has occurred. See the user guides for setup and the runtime compatibility notes for remaining live validation.
