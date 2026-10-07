---
status: accepted
---

# Support Claude Code as a review provider

Operators can choose Claude Code instead of Codex with `worker.provider`. Crow runs the official `claude` CLI in print mode with streamed JSON events and the operator's Claude subscription. As with Codex (ADR 0008), Crow does not call the model API directly, embed a provider SDK, or fall back to API keys (ADR 0001).

Each worker uses one provider. Workers paired with the same service may use different providers, so a provider outage cooldown applies only to the worker that reported it. A review keeps the provider it started with, like its model, and each saved session records its provider; a session cannot move between providers, so continuing one under another provider requires an explicit restart. Installations configured before this decision have no provider setting and keep using Codex.

## Isolation

Crow gives Claude Code its own `CLAUDE_CONFIG_DIR` with a separate subscription login, a private `HOME` and XDG directories, and a reduced environment without API keys. Every invocation passes:

- `--tools ""`, which removes every built-in tool, including shell, file editing, web access, and native subagents;
- `--strict-mcp-config` with a generated `--mcp-config` naming only Crow's inspection server;
- `--permission-mode dontAsk` with `--allowedTools` listing the inspection (and, for a parent review, delegation) tools, so anything else is denied without a prompt;
- `--setting-sources ""`, `--settings {"disableAllHooks":true}`, and `--disable-slash-commands`, so user, project, and local settings, hooks, and skills do not load;
- `--system-prompt` with Crow's reviewer instructions and `--json-schema` with the report schema.

`CLAUDE_CODE_DISABLE_CLAUDE_MDS` and `CLAUDE_CODE_DISABLE_AUTO_MEMORY` keep personal and directory instructions out of the review. Target-branch `AGENTS.md` and `.crow/review.md` reach the reviewer only through Crow's prompt, as with Codex.

Host-managed policy settings still apply and cannot be disabled from the command line. Claude Code reports the effective session in its first event, so Crow checks it before any tool runs: the offered tools must be Crow's inspection tools plus the structured-output tool, the only MCP server must be Crow's and connected, the permission mode must be `dontAsk`, the credential source must not be an API key, and the model must be the one requested. Any difference stops the review with a configuration or authentication error. Codex is verified differently, through `app-server` before the review starts.

## Models, sessions, and delegation

Claude Code has no model-listing command. Its SDK `initialize` handshake reports the account and available models without starting a conversation, so Crow reads the catalog there and caches it like Codex's. Aliases such as `opus` follow Anthropic's releases; the pinned model each alias resolves to is also accepted, for operators who want a fixed model. A model without adjustable reasoning uses the reasoning level `default`. A subscription login is checked with `claude auth status`; API-key and cloud-provider authentication are refused.

Each review has its own working directory, so Claude Code stores its session under a project directory unique to that review. `--resume` continues the saved session; Crow refuses to continue if Claude Code reports a different session. Retention removes expired sessions from `claude/projects`.

Delegation uses Crow's MCP tools for both providers. A delegated child is another Claude Code review with the same restrictions, no delegation tools, and the model Crow assigns.

## Consequences

Policy enforcement for Claude Code happens at session start rather than before the process launches; a violation ends the process before any tool call. Compatibility depends on the documented CLI flags and the stream-json event fields described above. `crow doctor --runtime` checks that the installed CLI supports every required flag; live behavior was checked against Claude Code 2.1.289 (see runtime validation).
