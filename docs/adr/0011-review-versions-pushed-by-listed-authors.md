---
status: accepted
amends: 0003, 0010
---

# Review versions pushed by listed authors

Author policy names whose PRs Crow reviews, and reviews use the operator's subscription. Checking only who opened a PR is not enough: anyone with write access can push to a PR's branch, and Git author fields can name anyone. So a listed author's PR could carry other people's changes into the operator's subscription usage, and into runtime experiments.

## Decision

For repositories with a selected author list, Crow reviews a new version automatically only when a listed author pushed it. The pusher comes from GitHub's repository activity for the PR's head branch, which records the authenticated user behind each push, force push and branch creation. Webhooks, catch-up, base-branch updates and dispatch all apply the check. A version pushed by anyone else is not reviewed, and the PR's status comment says who pushed it. If a listed author pushes again on top, Crow reviews the PR again, including the earlier commits.

Runtime experiments (ADR 0010) need more: every push since the branch was created must come from a listed author, and GitHub must still list that creation. Otherwise the review is inspection-only.

A requester can still ask for a review with `/crow review` or `crow review`. That is a deliberate choice, and such a review never runs code unless every push qualifies.

When Crow cannot confirm who pushed, it does not review automatically. This happens when GitHub lists a push late, when the lookup fails, or when the App cannot read a private fork. GitHub usually lists a push about a second after it, so webhook-triggered checks retry for a few seconds.

The operator can opt out per repository with `crow policy owner/repo --pushers anyone`. Reviews and experiments then follow the PR author alone. Repositories that review everyone are not affected.

## Alternatives considered

Commit author and committer fields are set by whoever creates the commit, so they prove nothing. Storing push webhooks would need new records and retention, and would still miss pushes from before enrollment or during downtime. Pull request webhooks arrive in no guaranteed order relative to push webhooks. GitHub's activity API is authoritative and available on demand.
