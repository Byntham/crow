# Runtime validation

The implementation was developed on Ubuntu using Node 24 and the official `codex-cli 0.154.0` executable. The probes below use a localhost provider fixture, synthetic authentication, isolated temporary state, and deterministic responses. They run the actual Codex CLI without consuming subscription usage or contacting a model for inference.

```sh
pnpm check
pnpm test:runtime
```

`pnpm check` first runs strict TypeScript checking, then builds fresh `.mjs` output in `dist/` and runs the JavaScript tests against that output. `pnpm test:runtime` also checks and builds before running the probes. The tests and probes deliberately exercise the modules and CLI paths that installation emits, including the inspection MCP entry point. They do not use a TypeScript execution loader.

Type checking covers `lib/*.mts` and `bin/*.mts`, including shared review, configuration, provider, and persistence contracts. Runtime guards remain necessary for incoming JSON, provider output, saved state, and SQLite rows. Build tooling and fixture tests remain JavaScript so the installation and update checks can run with Node alone. See [the migration design](typescript-migration.md) for the implementation sequence.

The ordinary suite covers durable SQLite receipts and jobs, authenticated service requests, safe bare-Git inspection, author and draft policies, merge-base comparisons, publication races, saved-report retries, pause/resume, pinned guidance, setup callbacks, HTTPS route preservation, encrypted backups, native installation, and retention.

The installed-runtime probes exercise:

- Effective configuration and the actual model-visible tool inventory, including Code Mode's nested tools.
- Fixed child model and reasoning settings through Crow-controlled delegation. Native subagent spawning is disabled.
- Initial and resumed structured reports using explicit saved session IDs.
- Interruption of the parent and delegated child, shutdown of their processes and response streams, and continuation of both original sessions.
- Isolation from personal and repository agent instructions. Crow requires a separate Codex home because the documented instruction-size flag alone did not suppress global instructions in this runtime.

The implementation streams provider events to retained local logs. It does not terminate a productive review because cumulative stdout exceeds a fixed buffer. Individual tool/process outputs remain bounded.

These checks do not establish real subscription entitlement, simultaneous refresh against OpenAI's production authentication service, model review quality, or successful external account onboarding. Those require the operator's subscription login and GitHub/Tailscale or Cloudflare authorization. A real PR publication remains a release-validation step after onboarding; it is not a feature of `crow setup`.

No live Crow service, public tunnel, GitHub App, or GitHub review was created during development. Existing gibo services and personal Codex settings were preserved.
