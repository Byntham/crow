---
status: superseded
---

# Connect workers through a shared GitHub App

Superseded by [ADR 0007](0007-make-each-crow-installation-independent.md). The following records the earlier shared-service decision, not the current deployment requirement.

Crow uses one public GitHub App and an operated connection service to deliver event-triggered review jobs to persistent workers on users' machines. Users install the existing App, select repositories, and pair a worker; workers connect outward, so users do not need repository workflow files, a GitHub Actions runner, or a public endpoint on their machine. The App can be distributed through a direct installation link without a Marketplace listing.

This trades an independently hosted installation for simpler user setup. Crow's maintainers must operate the service, protect the shared App private key, authenticate and authorize workers, persist jobs, and handle delivery failures. Provider subscription credentials remain on the worker; the service supplies short-lived GitHub installation credentials scoped to authorized repositories and operations, never the shared App private key.

The service receives webhook metadata and has the repository authority granted to the App. Fetching source and publishing findings directly between the worker and GitHub does not remove that trust relationship. The service also maintains GitHub status comments so it can report worker disconnections. Provider recovery, startup catch-up, and data retention decisions are recorded in `docs/design/review-execution.md`. The service audits failed webhook deliveries hourly by default, as well as on startup and recovery.

The initial deployment hosts both the connection service and the operator's worker on gibo. A unified Crow setup flow configures both services and arranges public HTTPS ingress through a supported provider, guiding the operator through any required browser authorization. Users connecting only a worker to the shared service still need no public endpoint. A gibo outage takes both roles offline, so GitHub status cannot be updated until service recovery.
