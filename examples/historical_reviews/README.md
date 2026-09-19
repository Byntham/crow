# Historical PR reviews

These scripts replay public GitHub PRs through Crow's production reviewer locally. They do not publish reviews or comments upstream. Repository code runs through Crow's container tools. The reviewer chooses its own setup and test commands using the original PR title/body and repository instructions.

`cases.json` includes seven real PRs and a labelled inverse of one fix. The inverse is a regression control, not an upstream PR. Its reviewer context uses a neutral description without disclosing the expected bug.

Repository guidance comes from the pinned target-branch tip, even when it differs from the comparison merge base or the comparison is reversed. Run the local regression check with `python3 examples/historical_reviews/test_prepare.py`; it uses local Git repositories without GitHub or model calls.

```sh
cargo build --example local_review
python3 examples/historical_reviews/prepare.py .crow-data/historical-reviews
python3 examples/historical_reviews/run.py .crow-data/historical-reviews --podman /path/to/podman
python3 examples/historical_reviews/export.py .crow-data/historical-reviews .crow-data/historical-reviews/results.json
```

Generated reports, receipts and screenshots belong under the ignored `.crow-data/` directory. Historical evidence is linked from its original commit in the [validation summary](../../docs/validation/historical-reviews.md).

The scripts require Python 3.11 or newer. Preparation requires authenticated `gh` access to public PR metadata. Reviews use the operator's configured model and normal inference billing. `local_review` uses a ten-minute review deadline, twelve runtime attempts, 120 seconds per command, 1.5 GiB memory, two CPUs, 256 processes and a 512 MiB workspace. These match the earlier autonomous validation settings, not the slightly smaller production memory/process defaults. Model decisions can vary between runs.

Use `--case ID` to select cases. Use `--attempt retry-N` to preserve the first attempt and run a new review after a Crow fix. Existing attempts are never overwritten. Original metadata, immutable comparison commits, raw reports and receipts remain under the output directory. The exporter omits provider reasoning transcripts and configuration. Passing shell commands, installed dependencies and static findings do not prove an application was tested. Inspect the receipts for the scope and outcome of each test.

To test recovery from an incomplete review, use `--attempt resumed --resume-from retry-N`. The runner copies the saved session and environments before resuming, preserving the original timeout or interruption. This grants another review turn with the same command and total experiment limits. The resumed attempt includes inherited receipts, which must not be counted as new executions. The runner pins its executable by content hash so rebuilding Crow cannot change an active replay's MCP helper.
