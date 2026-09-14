---
status: accepted
---

# Require subscription authentication for reviews

Confirmed during the Grill with Docs session: Crow must use the operator's coding-agent subscription, with no separately billed API-key substitute or automatic fallback. This rules out the API-key-based Codex GitHub Action example as Crow's default execution path, despite its simpler hosted setup. Crow must preserve provider authentication between runs and report when the operator needs to sign in again.

If subscription quota or authentication prevents reviews, Crow pauses affected work and retains pending jobs. It resumes when the provider permits usage and authentication is valid; it never switches to API billing to clear the queue.
