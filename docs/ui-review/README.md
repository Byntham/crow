# Crow interface review

Open [the before-and-after gallery](index.html). Each entry includes images and the captured text. Click an image to see the full output.

The gallery contains 73 comparisons. Use its search field to find a command or setup screen.

## Changes

| Area | Result |
| --- | --- |
| Command help | Every public command and option explains its purpose. Common workflows have examples. |
| Status | Repository and worker tables, reviews needing attention first, bounded recent history, readable dates, and recovery commands. A stopped worker cannot appear active just because its saved status says so. |
| Health checks | Pass/fail checklist with readable details, including nested provider diagnostics. Failure still returns a nonzero exit code. |
| Models and configuration | Model choices and reasoning levels are readable. Settings show units, defaults, and plain descriptions. Credentials stay hidden. |
| Repository settings | Common options accept flags such as `--model`, `--effort`, and `--timeout-seconds`. `--reset` restores worker defaults. Advanced JSON input remains available. |
| Review and repository commands | Results describe the action, current state, relevant settings, and next steps. |
| Lifecycle and maintenance | Start, stop, restart, draining, cleanup, updates, backup, and restore have readable confirmations. |
| Pairing | One set of labeled credentials with instructions for the worker machine. |
| Setup | Numbered choices explain machine roles, HTTPS access, GitHub ownership, model selection, and reasoning levels. Health and enrollment results use the same readable output as the CLI. |
| Setup browser pages | Responsive pages explain GitHub access, repository selection, successful connection, and recovery from invalid links or errors. |
| Installation and errors | Installation points to the stable command. Errors separate causes and connection failures suggest recovery commands. |
| Automation | `--format json` preserves structured results. Pairing and combined cleanup each emit one document. Update notices use stderr. |

## What remains

No public command was removed. The less common commands support separate workers, saved reviews, safe updates, or recovery. They now explain those purposes. The private inspection subprocess stays hidden from help, and release publishing remains in the separate maintainer executable.

The website, shell bootstrap installer, GitHub status comments, and GitHub review summaries already use prose, headings, or tables. They do not expose the internal JSON records, so their presentation remains unchanged. Provider login and service logs retain the underlying provider or journal output, with command help explaining how to use them.

## Capture method

Terminal images render output captured from the original and updated executables. Both use isolated temporary installations and the same sample repository, reviews, and local service responses. Full transcripts are adjacent to the images. The fixture pairing tokens have never belonged to a live service. Temporary paths and ports are normalized for comparison.

Browser captures show the actual page templates with sample values. Update-result images use a saved operation response passed to the production formatter; they do not perform an update. Setup captures stop before external registration or service installation. No GitHub comments were published and no live installation was changed.

`scripts/capture-cli.py` regenerates CLI captures from before/after binaries and the existing fake Codex test executable. The gallery includes help for every public command, plus the changed output and browser states.

Scripts that previously parsed default JSON must add `--format json`. The HTTP APIs and private inspection protocol retain their existing formats.

## Validation

The full test suite passed 191 tests. Two optional tests requiring an installed Codex runtime remain skipped. The new tests run the executable against local services and cover readable output, exact JSON results, hidden credentials, stale worker status, failed health checks, and command help. Formatting and Clippy passed with warnings treated as errors. Setup tests also passed after the final prompt formatting adjustment.
