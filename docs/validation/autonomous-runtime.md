# Live autonomous review validation

On 18 September 2026, the production reviewer completed three local model-driven reviews using the operator's configured gpt-6-astra model at high reasoning. Each repository had ordinary project files and no `.crow` review instructions. No test commands were supplied in the PR request. Crow used its automatic image and selected its own setup and investigation commands. No GitHub comments were published by these evaluations.

| Evaluation | Result |
| --- | --- |
| Checkout with a binary-only change hiding the payment label | Installed npm dependencies on both revisions, ran both smoke tests successfully, requested both screenshots as native image content, and reported the hidden label. |
| Harmless checkout graphic change | Installed dependencies, ran smoke tests, added desktop/mobile and delayed browser captures, viewed the artifacts, and reported no actionable findings. |
| Python version-sorting regression with an unrelated existing test failure | Installed application dependencies and pytest, compared the suite and focused version examples on both revisions, reported the introduced sorting bug, and identified the existing failure as unrelated. |

The Python experiment combined pytest and a successful diagnostic script in one shell command. Its command exit code was zero even though pytest reported a failure. The review correctly interpreted the output and compared that failure against base. Receipt statuses describe shell exit outcomes, not every nested test assertion.

The actual reports and command receipts are saved in [autonomous-runtime.json](https://github.com/Byntham/crow/blob/b5dfe995aa342db21ee11decde690761c2a19a31/docs/validation/autonomous-runtime.json). Image content is omitted from that JSON; the checkout screenshots are below. No model reasoning transcripts or credentials are included.

The linked reports and screenshots remain in the original validation commit. Generated evidence is excluded from the current source tree; save new runs under the ignored `.crow-data/` directory.

| Base | Head |
| --- | --- |
| ![Base checkout](https://github.com/Byntham/crow/blob/b5dfe995aa342db21ee11decde690761c2a19a31/docs/images/runtime-base.png?raw=true) | ![Head checkout with hidden label](https://github.com/Byntham/crow/blob/b5dfe995aa342db21ee11decde690761c2a19a31/docs/images/runtime-head.png?raw=true) |

These are controlled evaluations, not a representative benchmark of arbitrary repositories. The earlier checkout experiment used text-only pixel measurements; this evaluation used `read_artifact`, which returns actual MCP image content. A separate native MCP transport test verifies that screenshots are not flattened into JSON text.

Reproduce the visual defect and clean control with `examples/visual_checkout/prepare.py --autonomous`, adding `--clean` for the control. Reproduce the Python case with `examples/autonomous_python/prepare.py`. Run each generated `source.json` through `examples/local_review.rs` with image `auto`. Model decisions and wording can vary.
