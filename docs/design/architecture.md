# Architecture

Crow is a Linux daemon and CLI in a single Rust executable. The same binary runs the CLI, the connection service, the worker, and the private `_inspection-mcp` command that serves repository tools to a review. The static website and shell bootstrap are plain assets.

## Components

- **Connection service** (`service.rs`, `store.rs`, `github.rs`): receives GitHub webhooks, owns the GitHub App credentials, repository policy, durable jobs, and GitHub status comments, and publishes completed reports. It runs on a dedicated single-thread Tokio executor, so synchronous reads and decisions between awaits cannot interleave, while network waits stay concurrent.
- **Worker** (`worker.rs`): claims jobs over HTTPS, fetches pinned source into a bare Git repository, runs the review through a provider, saves the validated report, then hands it to the service.
- **Providers** (`provider/`): drive the official Codex or Claude Code CLI with the operator's subscription login. `provider/mod.rs` holds the shared review driver (sessions, prompts, events, report validation); `codex.rs` and `claude.rs` hold each CLI's isolation, policy checks, and event parsing. See ADR 0008 and ADR 0009.
- **Inspection and delegation** (`inspection.rs`, `delegation.rs`): the MCP server a reviewer uses to list, read, search, and diff pinned Git objects, and to run bounded child reviews.

Tokio owns asynchronous processes and network tasks; Axum handles inbound HTTP; reqwest with rustls handles HTTPS; rusqlite stores durable state in bundled SQLite.

Persisted records and wire envelopes are `serde_json::Value`s validated at each boundary. This keeps configuration version 1, the SQLite records/receipts/events schema, camelCase HTTP payloads, saved job and session files, and finding identifiers compatible across releases, including the distinction between missing and null fields.

## Required invariants

- A webhook receipt and minimal event commit together before acknowledgment.
- A job has one active lease. Recheck ownership and policy after network waits.
- Preserve incomplete provider sessions. Never silently restart a lost session.
- Save a valid complete report before publishing. Reconcile authenticated bot metadata before retrying publication.
- Repository code is data. Inspect pinned Git objects without checkout, hooks, external diff helpers, or repository commands.
- Enforce provider tools and delegation limits before every new or resumed session.
- Keep author policy, command requester policy, and worker/admin authentication separate.
- Keep initial backlog exclusion, held catch-up batches, and no scheduled PR polling.
- Clean up process groups and join background work before releasing runtime ownership.
- Never claim live subscription or network behavior from synthetic test results.

## Alternatives considered

Calling model APIs directly instead of the official CLIs would change the authentication product and recovery semantics, and would require separately billed API keys (ADR 0001). A provider SDK or a maintained CLI fork would add a dependency that must track each provider's releases. Typed schemas for every persisted record would add a migration for existing installations without changing behavior.
