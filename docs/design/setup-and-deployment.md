# Setup and deployment design

This document describes the intended setup experience, not an installation guide for the current implementation. Each user hosts an independent Crow installation. The earlier official shared-service rollout is superseded by ADR 0007.

## One setup flow

Use one installer and one `crow setup` command with three roles:

| Role | Setup responsibilities |
| --- | --- |
| Both, default | Configure the operator's connection service, local storage, worker, public HTTPS, GitHub App onboarding, Codex authentication, and persistent startup on one machine. |
| Connection service only | Configure public HTTPS, the operator's GitHub App, storage, and persistent startup. This host does not need Codex. |
| Worker only | Pair with the operator's existing connection service, verify local Codex and subscription login, and configure persistent startup. This host does not need public HTTPS or the App private key. |

Each installation registers and owns its own GitHub App. Crow guides registration through GitHub's manifest flow and captures generated credentials after browser confirmation. App installation and repository selection remain visible operator choices. No Marketplace listing or manual copying of App keys is required by this flow.

Crow should detect existing setup, preserve it on reruns, and guide browser steps through URLs usable from another device. Combined setup must not require separately deploying the connection service or manually assembling the recommended networking path. Supporting separate hosts does not add multiple-worker scheduling or session transfer to the first release.

Setup selects the authenticated provider's reported default model and reasoning level, displays the selections, and lets the operator change them. Crow then stores explicit selections so later provider-default changes do not silently change configured reviews. Model discovery failure follows the cached-catalog behavior in the review execution design.

Assume the hosting machine has no browser. When Codex login is needed, prefer its device-code flow: Crow displays the URL and code on the host, the operator signs in on a separate desktop, and the host completes authentication. Account or workspace settings may need to enable device-code login. Document SSH forwarding of the localhost callback as a fallback; opening a host-local callback URL on another desktop is not sufficient by itself. This follows [OpenAI's headless authentication guidance](https://learn.chatgpt.com/docs/auth). No browser is required on the worker.

Setup finishes with connection and configuration checks, including public HTTPS, GitHub permissions, worker connectivity, provider authentication, and persistent startup. It does not run or offer a test PR review. Runtime compatibility checks must also avoid starting a review during onboarding.

## Public HTTPS

Each installation needs its own stable public HTTPS route for GitHub webhooks. The core service accepts a configured public base URL and does not depend on a specific networking provider. A worker on another machine connects outward to its own installation's service.

Guided Tailscale Funnel setup is the default public HTTPS path and is included in the first release. Funnel provides a public hostname without a purchased domain or router forwarding, but requires Tailscale authorization, remains beta, and has bandwidth limits. Crow must preserve existing private Serve routes by using a dedicated unused supported port and managing only its own binding.

Cloudflare Tunnel and an existing public HTTPS endpoint remain alternatives. Temporary quick tunnels are unsuitable for a GitHub App's stable webhook URL. Independent hosting removes reliance on another Crow operator, but Tailscale Funnel and Cloudflare Tunnel still rely on their networking providers. Private Tailscale or Serve alone cannot receive GitHub's public webhooks.

## Gibo deployment

The operator's own connection service, storage, and worker run on gibo. The previously selected Cloudflare hostname `connect.birdapp.dev` can serve this personal installation; it is no longer a required endpoint for other users. Domain registration is complete, but DNS onboarding has not been verified and no tunnel or DNS changes have been made during the interview.

Cloudflare supplies a route to gibo rather than hosting Crow's backend. Named-tunnel configuration and native service startup can be automated after account authorization; the exact onboarding mechanism still needs implementation validation. Normal HTTPS proxying terminates TLS at Cloudflare, so webhook metadata passes through its infrastructure. Codex credentials and sessions stay on workers.

An outage affects only the installations hosted on that machine. A stopped connection service cannot update GitHub until it recovers, and its tunnel is not a durable webhook queue. Worker connections reconnect after transport interruptions; delivery audits and catch-up repair missed work.

## Access and operations

The operator who enrolls a repository controls its worker assignment, author policy, and review settings. Enrollment verifies their GitHub authority. Eligibility to receive or request reviews does not confer configuration access. Team administration is outside the first release.

There is no required central registration or official-service invitation list. Independent installations still authenticate paired workers, verify webhook signatures, and enforce repository authorization and author policy. The Crow source repository remains private until the implementation works.

Provide manual encrypted export and restore commands for configuration, GitHub App credentials, and the connection-service database. Backups are optional and add no onboarding step. Automatic scheduled backups can come later. Document how to protect the archive and its decryption secret, restore consistent service state, and reconcile restored jobs with workers and GitHub before dispatch. A service backup does not transfer local provider sessions between machines. Each operator owns their backup; it has no dependency on an official Crow backend.

## Initial host assessment

A read-only inspection of gibo on 2026-09-13 found an Intel i5-4590 with four cores, approximately 30 GiB total RAM with 19 GiB available, 202 GiB available on the root filesystem, and load averages of 0.11, 0.08, and 0.06. This is a hardware snapshot, not a Crow benchmark. It reveals no obvious capacity obstacle for the operator's initial installation.

Other users' independent installations impose no connection-service or review workload on gibo. An operator can later relocate their own connection service and database while keeping workers on their original machines. This does not require transferring partially completed review sessions.

## Required user documentation

Lead with a combined self-hosted Linux quickstart. Also provide worker-pairing and separate-machine guides. Explain each role, public HTTPS prerequisites, guided GitHub App registration, headless browser steps, and verification of a complete working connection.

Document configuration defaults and overrides, updating, retention and recovery, and troubleshooting for networking, pairing, GitHub permissions, Codex authentication, instruction isolation, and service startup. Keep provider-specific details in their relevant guides. The recommended independent setup must be fully guided and well documented.
