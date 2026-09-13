---
status: accepted
---

# Make each Crow installation independent

Each user hosts their own complete Crow installation, including a connection service and their own GitHub App. The normal setup runs the service and worker together on one machine; separate-role deployments remain supported. Reviews do not depend on gibo or another official Crow backend. This supersedes ADR 0006's shared service as the normal deployment model.

Independent operation adds public HTTPS setup and GitHub App registration for each operator. Crow must guide those steps through one setup flow, including App manifest registration and automatic credential capture after browser confirmation. The operator retains the App private key in their own connection service; workers receive only appropriate scoped credentials. Marketplace publication is not required.

This tradeoff prioritizes independence and local ownership over the shortest possible worker-only onboarding. Guided Tailscale Funnel is the default ingress path in the first release, while Cloudflare and configurable HTTPS remain alternatives. Tunnel selection does not change which machine hosts the service.
