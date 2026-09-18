# Visual checkout experiment

This manual end-to-end experiment runs Crow's real reviewer against a browser application. It requires the operator's existing Crow provider configuration, rootless Podman, Python 3, Git and Rust. Model calls use the configured provider. No GitHub review is published.

The preparation script creates a separate two-commit repository. Only a binary PNG changes. In the second commit, the graphic covers the payment label even though the button remains accessible and functional. The generator and this explanation stay outside the reviewed repository.

From Crow's repository root:

```sh
python3 examples/visual_checkout/prepare.py --autonomous .crow-data/visual-checkout-demo
cargo run --example local_review -- \
  .crow-data/visual-checkout-demo/source.json \
  .crow-data/visual-checkout-demo/crow-run \
  auto \
  /absolute/path/to/podman
```

`--autonomous` adds ordinary npm scripts and a real dependency, and removes Crow-specific review instructions. Crow provisions its toolchain, reads the project, chooses setup commands and tests, and can inspect captured PNGs through its image tool. The output directory must be new.

Add `--clean` when creating a separate fixture to change the graphic without hiding the label. This provides a false-positive control. See the actual [live validation results](../../docs/validation/autonomous-runtime.md).

Application sources are exported to the output's `base/` and `head/` directories. To inspect either manually, run `npm install` and `npm start` in that directory, then open port 8080. It is a fake checkout and processes no payments.

The runner writes `result.json` and durable experiment receipts under `crow-run/reviews/visual-checkout-review/experiments/`. Requested screenshots are retained in its `artifacts/` subdirectory. A command exiting successfully does not establish that the application has no bugs; both provided smoke tests pass in the broken-label fixture.

For comparison with the original experiment, omit `--autonomous` to create the earlier fixture with neutral `.crow` guidance and no npm dependency. The adjacent Containerfile is a minimal browser image for that offline fixture. The runner also accepts an immutable image ID instead of `auto`; custom images need the documented preparation helpers if dependency installation is required.
