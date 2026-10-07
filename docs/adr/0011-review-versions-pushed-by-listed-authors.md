---
status: accepted
amends: 0003, 0010
---

# Review versions pushed by listed authors

Author policy names whose PRs Crow reviews, and reviews use the operator's subscription. Checking only who opened a PR is not enough: anyone with write access can push to a PR's branch, and Git author fields can name anyone. So a listed author's PR could carry other people's changes into the operator's subscription usage, and into runtime experiments.

## Decision

For repositories with a selected author list, Crow reviews a new version automatically only when a listed author pushed it. The pusher comes from GitHub's repository activity for the PR's head branch, which records the authenticated user behind each push, force push and branch creation. Webhooks, catch-up, base-branch updates and re-queues of superseded versions apply the check whenever they queue a version; Crow looks pushes up only for those decisions and for experiments. A version pushed by anyone else is not reviewed, and the PR's status comment says who pushed it. If a listed author pushes again on top, Crow reviews the PR again, including the earlier commits.

Runtime experiments (ADR 0010) need more: every push since the branch was created must come from a listed author, and GitHub must list both that creation and the push of the current head. Otherwise the review is inspection-only.

A requester can still ask for a review with `/crow review` or `crow review`. That is a deliberate choice, and such a review never runs code unless every push qualifies.

No webhook waits on this lookup, so one PR cannot hold back its repository's events. If GitHub refuses it for good, with a 404, 410, 422 or 451 as for a private fork the App cannot read or one taken down, or lists the push without an account, the version is skipped as unconfirmed. If GitHub cannot say yet, because the push is not listed or the lookup fails for now (403s included, which can be secondary rate limits without headers), the version is queued as unverified. When a worker claims it, dispatch checks again. A listed author's push goes ahead, anyone else's is skipped, and otherwise it waits: from that first check, up to five minutes for GitHub to list the push, which usually takes a second, and up to an hour for lookups to recover. A version superseded before then stays unverified. After that it is skipped as unconfirmed. A version that was already queued, requested or reviewed keeps that state when later webhooks arrive for it, and re-queueing or restarting the same version keeps its earlier admission. For an admitted version, a failed lookup at dispatch only makes the review inspection-only.

The operator can opt out per repository with `crow policy owner/repo --pushers anyone`. Reviews and experiments then follow the PR author alone. Repositories that review everyone are not affected.

## Alternatives considered

Commit author and committer fields are set by whoever creates the commit, so they prove nothing. Storing push webhooks would need new records and retention, and would still miss pushes from before enrollment or during downtime. Pull request webhooks arrive in no guaranteed order relative to push webhooks. GitHub's activity API is authoritative and available on demand.
