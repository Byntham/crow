# Runtime status comment examples

These examples show the wording added to Crow's existing main status comment. Counts are command attempts, not individual assertions. A command can run hundreds of assertions, and commands on both base and head count separately.

## During an investigation

| Status | Commit | Review trigger |
| --- | --- | --- |
| Reviewing | `1234abcd` | Pull request event |

**Runtime testing:** Running tests. Test commands: 1 passed, 1 running. Setup attempts: 2 passed.

## After a completed review with mixed results

| Status | Commit | Review trigger |
| --- | --- | --- |
| Completed | `1234abcd` | Pull request event |

**Runtime testing:** Finished. Test commands: 2 passed, 1 failed. Setup attempts: 2 passed. Failed commands can include base failures and investigation attempts; see the review for confirmed bugs.

## Other outcomes

| Situation | Runtime testing text |
| --- | --- |
| Worker has not enabled execution | Disabled by worker configuration. |
| Reviewer is still inspecting | Reviewer is deciding what to test. |
| Dependency setup is running | Setting up the test environment. Setup attempts: 1 running. |
| Setup failed and no tests ran | Testing blocked or interrupted during setup. Setup attempts: 1 failed. |
| Review finished without test commands | Not attempted by the reviewer. |
| Review stopped during a command | Stopped. Test commands: 1 interrupted. |
| Test sandbox could not start | Finished. Test commands: 1 blocked. |

Updates come from worker receipts over the existing heartbeat and final-report protocol. The service checks the worker's active lease before accepting them. Test coverage verifies live updates while a provider is waiting, setup/test separation, final counters, invalid payload rejection, and protection against late updates overwriting a completed review.

## Failure locations and warnings

The same status line now includes the current step while a command runs:

> **Runtime testing:** Setting up the test environment. Setup attempts: 1 running. Current step: dependency setup command.

A setup failure identifies its location:

> **Runtime testing:** Testing blocked or interrupted during setup. Setup attempts: 1 failed. Failure locations: 1 at dependency setup command. Worker receipts contain the error details.

A successful test with an unavailable screenshot remains a successful command:

> **Runtime testing:** Finished. Test commands: 1 passed. Warnings: 1 at screenshot collection. Worker receipts contain the error details.

Cache restore/save and container cleanup failures also appear as warnings. Public comments include only known stage names and counts. Commands, output and host errors remain in the worker's `reviews/<job-id>/experiments/<experiment-id>.json` receipts. Receipts include the pinned commit, command, exit status, bounded beginning/end of output, stage, failure location and artifact results.

Maintenance failures outside an experiment appear in `crow status` and `crow status --format json`. The data directory's `runtime-maintenance.json` contains the last maintenance result. `crow cleanup` retries maintenance explicitly. Failed cleanup retains ownership records so a later attempt can find the resource.
