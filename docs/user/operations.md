# Configuration and operations

Use `crow help` for command syntax. Configuration changes take effect after `crow service-restart`; stopping preserves interrupted work where the provider has saved a usable session.

Commands show readable summaries by default. `crow status` lists repositories, workers, and recent reviews; `crow doctor` shows which checks passed and what needs attention. Review commands show the resulting state and relevant next steps.

For scripts, add `--format json`, for example `crow status --format json`. This returns the structured result, including details omitted from the summary. Update notices go to stderr so they do not interrupt JSON output. `pair` and `cleanup` each return one JSON document. Interactive setup, login, installation, foreground execution, and streaming logs use text output.

## Review policy and settings

```sh
crow enroll owner/repo
crow policy owner/repo --authors alice,bob
crow policy owner/repo --everyone
crow policy owner/repo --requesters alice,bob
crow config
crow models
crow config worker.concurrency 3
crow config worker.model provider-model-id
crow config worker.effort high
crow config worker.subagents '{"mode":"inherit","max":8}'
crow config worker.retry '{"mode":"fixed","count":10,"delayMs":5000}'
crow repo-config owner/repo --model provider-model-id --effort high
crow repo-config owner/repo --timeout-seconds 1800
crow repo-config owner/repo --reset
```

Author policy determines whose PRs Crow can review. Requester policy separately determines who can trigger `@crow review`, `@crow resume`, `@crow restart`, or `@crow pause` in a PR comment; it defaults to the operator. Granting someone request permission does not authorize their own PRs or give them configuration access. `--requesters` changes only requester permission, and the target PR must still pass author and draft checks.

`crow config` hides credentials. Model names and reasoning levels come from `crow models`; Crow does not maintain a model list in its source. If retrieval fails, Crow marks the last successful catalog as cached and reports the error. Saved explicit selections do not change when provider defaults change.

Model and effort settings accept plain text. Numbers, booleans, and nested settings use JSON values. `crow config worker.model null` restores the provider default. Use `crow config --format json` to inspect the full configuration with credentials hidden.

Workers default to three active PR reviews, with up to eight subagents each. The subagent limit is a ceiling. `inherit` uses the parent model and effort; `configured` requires `model` and `effort` in the subagent settings. There is no mode allowing a reviewer to choose its own model policy.

On a service-only host, override settings remain pending provider validation until the assigned worker starts a review. Invalid settings pause work with an actionable error; Crow does not substitute another model.

Repository overrides support `model`, `effort`, `subagents`, `retry`, and `timeoutMs`. A timeout of zero means no fixed runtime limit. Progressive retries use delays of 5 seconds, 15 seconds, 30 seconds, 1 minute, 2 minutes, then 5 minutes. Longer provider waits and shared outage cooldowns take precedence. Quota and authentication failures pause work without an API-key fallback.

Each `repo-config` call replaces that repository's overrides; omitted settings use worker defaults. Use `--reset` to remove all overrides. Advanced settings remain available through `--json`, for example `crow repo-config owner/repo --json '{"retry":{"mode":"fixed","count":3,"delayMs":5000}}'`. This input option is separate from the `--format json` output option.

Optional `.crow/review.md` holds review-specific guidance. It is not generated during setup. Normal coding agents are not instructed to read it. Crow also reads applicable `AGENTS.md` files. Both come from the pinned target branch so a PR cannot replace its own review instructions. Rule changes apply to the next review; they do not trigger a backlog automatically.

## Work and recovery

```sh
crow review owner/repo 123
crow pause owner/repo 123
crow resume owner/repo 123
crow resume owner/repo 123 --model provider-model-id --effort high
crow restart owner/repo 123
crow catch-up owner/repo
crow catch-up owner/repo --include-backlog
crow release owner/repo
```

`resume` continues saved work when the comparison is still current. A review paused before it started can resume without a session. An interrupted review with no usable session requires a restart. `restart` explicitly begins fresh work. `review` also requests a fresh review of a previously completed comparison. Duplicate active requests coalesce.

Initial open PRs remain excluded across restarts unless they receive a qualifying event or you request their inclusion. Startup/recovery catch-up is enabled by default. Batches above the configured threshold of ten are held until you run `crow release`. Change this with `crow config catchUp.threshold 20`; disable automatic catch-up with `crow config catchUp.enabled false`.

A new PR revision supersedes unfinished old work. Completed reports record the actual head and merge base. A later summary links to earlier findings with a brief status so their omission does not imply a fix. Only reassessed findings can be described as fixed or still present.

Crow keeps one editable status comment on each enrolled PR. It records the latest state, commit, and review trigger. New revisions and manual commands update that comment instead of adding another status comment. Commands are accepted only in normal PR conversation comments, not inline review comments. Malformed commands and commands from unauthorized users are ignored.

GitHub gets concise reviewing, retrying, paused, and completed status. Detailed diagnostics stay on the host. A failed review does not publish partial findings. A completed report survives a GitHub publication failure and retries publication without repeating inference.

## Lifecycle and updates

`crow drain` holds new review claims while active reviews finish. `crow undrain` restores claims. Run these commands on the connection-service host. Setup and updates wait for model review work to finish, but do not wait for delayed GitHub publication retries. Completed reports remain in the database and publication resumes after restart without repeating the review. Setup and updates restore the drain they requested even if restarting fails, and preserve an existing operator drain. If both restart and drain cleanup fail, restore service access and run `crow undrain`.

```sh
crow status
crow doctor --runtime
crow logs
crow stop
crow start
crow service-restart
crow update
crow cleanup
```

`crow status` checks for updates at most once per day and reports available versions. All installations check the public stable version at `downloads.birdapp.dev/latest.txt`. Offline update checks do not prevent status output. Download checks do not require GitHub authentication.

`crow start` and `crow service-restart` wait until the process reports readiness. Stopping an active review preserves its saved session; use `crow resume` to continue it.

The native user systemd service starts after reboot and continues after logout. `crow run` runs in the foreground for another service manager.

`crow update` downloads the published stable release for the host architecture from `downloads.birdapp.dev` and verifies its SHA-256 checksum and reported version before stopping the service. It drains active work, switches the installed executable, and restarts Crow. It verifies that the new process finished initialization and restores the previous executable if startup fails. There is no source checkout or local build step. Only an explicitly published stable version is an update candidate. The hosted endpoint must be deployed before this update path is available.

Updates do not modify source checkouts or replace your Codex installation. If an update fails, inspect the reported error before running `crow start`.

Session/diagnostic retention defaults to seven days after completion, supersession, or PR closure. Paused sessions remain while their comparison is relevant. Compact review records remain while the repository is enrolled. Cleanup does not delete GitHub comments.

## Encrypted backups

Create a strong passphrase in a private file. Keep that file separate from the encrypted archive. Do not put the secret directly in shell arguments or source control.

```sh
crow backup /safe/location/crow.backup --passphrase-file /private/crow-backup-secret
crow stop
crow restore /safe/location/crow.backup --passphrase-file /private/crow-backup-secret
crow start
crow doctor --runtime
```

Backup includes Crow configuration, GitHub App credentials, and a consistent snapshot of the service database. Existing archives are not overwritten. Restore requires Crow to be stopped, validates the archive and database before replacing state, and marks restored jobs for reconciliation with GitHub and workers before dispatch.

Provider authentication and sessions are not backed up or transferred. A restored worker may need `crow login`; jobs whose sessions are unavailable need an explicit restart. Tunnel account credentials and tunnel configuration files are also separate from this service backup. Restore the networking provider's configuration or configure HTTPS again on the destination. Automatic scheduled backups are not included.

## Troubleshooting

- A headless Codex login uses a URL and device code on your desktop. If your account disables device login, enable it in account/workspace settings. The alternative browser flow needs SSH forwarding of the host's localhost callback; merely opening its localhost URL on another desktop will not work. See [Codex authentication](https://learn.chatgpt.com/docs/auth).
- If GitHub enrollment fails, verify the App is installed on the selected repository and `gh auth status` reports the Crow operator. Enrollment requires repository admin or maintain authority.
- A worker that cannot connect needs the service URL and its own pairing token. Private Serve alone does not make GitHub webhook delivery possible.
- Funnel setup preserves existing routes. If all supported ports are occupied, free a port or choose Cloudflare/existing HTTPS. Account-level Funnel permissions may require a tailnet administrator.
- `crow doctor --runtime` checks provider capabilities without inference. It cannot prove live concurrent authentication refresh or provider interruption behavior.
- For systemd errors, run `crow logs`, verify the installed executable still exists, and ensure the user has lingering enabled. Do not use `sudo crow setup` to fix a user-service permission error.
