---
status: accepted
---

# Keep each PR's reports in one comment

Crow used to publish every completed report as a new PR review. Each one repeated the summary, every finding and a growing list of earlier findings and reviews, so a PR that went through many review cycles filled its conversation with near-duplicate reports.

## Decision

Each PR has one Crow comment, and every status change and every new report edits it. GitHub keeps its edit history, so earlier reports stay readable there. The comment changes only when what it shows changes, so that history holds reports rather than timestamps.

When a review completes, the comment shows its report. A headline states the result, such as "🟡 3 findings on `8cb88f8`", using the colour of the most severe finding. Each finding follows, most severe first, folded to its severity and title; opening it shows its location, linked to the reviewed commit, a link to its inline thread when it has one, and its full text. The reviewer's summary follows as folded review notes, then a row with one dot per published report, coloured the same way and linked to that report's inline review or commit, and a small line with the comparison base, target, model, trigger and time. While a later review is queued, running or paused, the comment shows that state above the last report, folded, so its findings don't read as current. A comparison that was reviewed before, such as after a force-push back to a commit or a base branch switched back, isn't reviewed again; the comment says that its report is in the edit history. If the links would make the comment longer than GitHub allows, the report leaves them and the dots out; validation bounds the report without them.

The comment no longer lists earlier findings. Reviews don't reassess them, so the list could never say which were fixed, and it grew with every cycle. Their inline threads track them instead, where people and coding agents reply to and resolve each one. So that leaving a finding out isn't read as fixing it, the history row states how many earlier findings the report doesn't repeat and that they were not rechecked. Folding costs coding agents nothing, since they read the comment's Markdown, where folded text is still present.

Findings that are new and anchored on lines the PR adds still get inline comments, posted before the comment is written so the report can link their threads. Those comments go in a review whose visible body is one line pointing to the comment. A report with no new anchored findings posts no review.

The comment carries the report's machine-readable marker, by which Crow recognizes an already reviewed comparison, as it does from the full reports that earlier releases posted as reviews. The inline review's marker names only its job, so that a retry doesn't post the same inline comments twice. It doesn't mark the comparison as reviewed, because only the comment publishes the report.

## Alternatives considered

Dropping inline comments would leave the conversation with one Crow comment, but replies about a finding would then have no thread. Posting each report as a new comment and deleting the previous one would lose its history, and would also move Crow's comment to the bottom of the conversation each time. A dashboard of tables, a comment that only links to the inline threads, and a report with nothing folded were also compared: the tables are cramped on phones, the link-only comment leaves agents without the findings' text, and the unfolded report is long to scroll.
