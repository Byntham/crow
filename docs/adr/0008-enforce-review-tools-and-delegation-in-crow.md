---
status: accepted
---

# Enforce review tools and delegation in Crow

Crow runs the installed official Codex executable through `codex exec`. It supplies a dedicated subscription-authenticated Codex home and a controlled working directory. Source stays in a bare Git repository and is available only through Crow's inspection MCP tools. The provider receives no repository checkout from which to discover PR-controlled configuration or invoke repository scripts.

On Codex 0.154.0, a local protocol probe found that setting `project_doc_max_bytes=0` did not suppress global AGENTS.md. The same probe found native subagent calls could override custom-role model settings. Crow therefore uses its own MCP delegation tools to start bounded child `codex exec` sessions with fixed settings. The parent decides which tasks to delegate, while Crow enforces Inherit or Configured model policy and the concurrent-task ceiling. Children cannot delegate again. Saved task state supports explicit continuation and replacement without treating interrupted work as completed.

Some models require Codex's sandboxed Code Mode host to invoke tools. Crow permits that orchestration runtime but excludes the built-in `functions` tool namespace. This removes the otherwise hidden file-edit tool as well as host execution tools. The only nested tools available to a child reviewer are Crow's file listing, reading, literal search, and diff tools. The parent additionally receives bounded delegation tools and the provider's clock tools. Crow verifies effective configuration before starting or resuming a review.

This trades a small amount of provider-adapter code for enforcement of the agreed model and inspection policies. It does not add a maintained Codex fork, a provider SDK, or an API-key fallback. Compatibility must be checked against the operator's official runtime as it changes. See [runtime validation](../design/runtime-validation.md).

ADR 0009 adds optional Crow-managed experiment tools to the main reviewer. The provider still has no host execution tools, and child reviewers remain inspection-only.
