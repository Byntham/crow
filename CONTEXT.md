# Crow

Crow is an automatic pull request reviewer that uses the operator's coding-agent subscription and publishes advisory findings on GitHub.

## Language

**Connection service**:
The part of an operator's Crow installation that receives GitHub events and delivers review jobs to its workers. It manages that installation's GitHub connection while workers perform reviews.
_Avoid_: Reviewer when referring to the connection service

**Crow installation**:
An independently operated Crow deployment with its own GitHub App, connection service, and workers. Its reviews do not depend on a centrally hosted Crow service.
_Avoid_: Worker when referring to the complete deployment

**Worker**:
A persistent component on the operator's machine that performs reviews using the operator's provider subscription.
_Avoid_: GitHub Actions runner

**Review job**:
A request for a worker to review a pull request revision. A review job can wait for an available worker before a review run begins.
_Avoid_: Review run when referring to work that has not started

**Enrolled repository**:
A GitHub repository whose incoming pull requests the operator explicitly selects for Crow to review, including pull requests from forks. Enrollment is independent of whether the repository is public or private.
_Avoid_: Private repository when referring to an enrolled repository

**PR author**:
The GitHub user who opened a pull request. The PR author can differ from the person who pushes a later commit or reruns a review.
_Avoid_: Triggering user when referring to authorship

**Review run**:
One attempt by Crow to assess a particular pull request revision and produce findings.
_Avoid_: Review when distinguishing execution from the published feedback

**Review subagent**:
An agent delegated part of the analysis within one review run. Several review subagents can work in parallel on the same pull request without starting separate review runs.
_Avoid_: Worker when referring to an agent within a review run

**Review session**:
The provider conversation associated with a review, whose saved context can support continuing the analysis after an interruption. Its existence does not mean the review completed or that every interrupted operation can be recovered.
_Avoid_: Review report when referring to saved analysis context

**Reviewed commit**:
The specific pull request commit a review report assessed. A report identifies this commit so readers and Crow can distinguish its findings from feedback on a later revision.
_Avoid_: Latest commit when referring to the commit assessed by an existing report

**Comparison base**:
The common ancestor of the PR's source and target histories used as the starting point for its proposed changes. This is the merge base, which can remain unchanged when the target branch advances.
_Avoid_: Target branch tip when referring to the comparison base

**Initial backlog**:
The existing pull requests present when a repository is enrolled. Crow leaves these alone by default until a later qualifying event or explicit review request; the operator can choose to include them during enrollment.
_Avoid_: Recovery work when referring to PRs that predate enrollment

**Catch-up**:
Discovery of eligible work missed while Crow was unavailable, performed at startup or recovery, or requested manually. Catch-up respects the operator's choice about the initial backlog.
_Avoid_: Scheduled polling

**Finding**:
A specific, actionable problem introduced or exposed by a pull request, supported by code evidence, a relevant location, and a meaningful consequence or explicit project-rule violation. The report gives a coding agent enough information to verify the problem and evaluate a correction; severity alone does not determine whether a finding qualifies.
_Avoid_: Comment when referring to the issue rather than its presentation on GitHub

**Review report**:
Crow's feedback on a pull request, written primarily for a coding agent to investigate and address. It is advisory and does not constitute approval or a requirement to block merging.
_Avoid_: Merge decision

**Review rules**:
Optional project-specific guidance for how Crow reviews pull requests, including review emphasis, exclusions, and reporting criteria. Review rules supplement shared engineering conventions rather than replacing them.
_Avoid_: Agent permissions when referring to review criteria

**Provider**:
The coding-agent product that performs Crow's analysis, such as Codex or Claude Code.
_Avoid_: Model when referring to the coding-agent product

**Review model**:
The model selected within a provider to perform review analysis.
_Avoid_: Provider when referring to the model choice

**Reasoning level**:
The provider-supported reasoning-effort setting selected for a review model. Available levels depend on the provider and model.
_Avoid_: Review severity, which describes findings rather than analysis effort

**Subscription account**:
The authenticated provider account whose usage allowance a review run consumes. It is distinct from the PR author's GitHub account.
_Avoid_: GitHub account when referring to the source of provider usage

**Author allowlist**:
The set of GitHub users whose pull requests are eligible for automatic review.
_Avoid_: Contributor list, which does not express review eligibility

**Author policy**:
A repository's rule for whose pull requests are eligible for review: only the operator, selected GitHub users, or everyone. Eligibility by author is separate from whether a pull request targets an enrolled repository.
_Avoid_: Repository visibility when referring to review eligibility

**Fork PR**:
A pull request whose source branch belongs to a different repository from its target. Crow evaluates it under the enrolled target repository's author policy; the source fork does not need separate enrollment.
_Avoid_: Unwatched repository when referring to an unenrolled fork supplying a PR to an enrolled target
