# Public HTTPS

GitHub must reach Crow's webhook endpoint from the public internet. The worker makes outbound connections to the connection service; it does not require a separate public port.

The connection service binds to localhost by default. Its public origin routes `/webhooks/github`, worker requests, authenticated administration, and temporary GitHub App onboarding. Webhook signatures and worker/admin credentials protect those endpoints.

## Tailscale Funnel

Choose `funnel` during setup. Crow can install Tailscale on supported Linux machines and guide account authorization. Funnel supplies a stable public `*.ts.net` HTTPS address. You do not need to buy a domain or configure router forwarding.

Funnel differs from private Tailscale Serve. Serve is reachable by authorized devices on your tailnet. Funnel accepts public requests, including GitHub webhook deliveries. Other applications can continue using Serve on the same host.

Crow checks current Serve configuration before choosing a port. It prefers 8443, then 10000, then 443. It reuses a recorded Crow binding only if that port still contains exactly Crow's proxy route. It never resets the host's Serve configuration. If all supported ports are occupied, setup stops and asks you to free a port or choose another HTTPS provider.

The `--bg` binding survives Tailscale restarts. Crow does not run a recurring command to rebuild the route. Funnel remains a Tailscale service with its own availability, account policies, beta status, and bandwidth limits. A private Serve route alone cannot receive GitHub webhooks.

If a Tailscale command fails because of missing system permissions, setup offers to retry it with sudo. You can decline, run the displayed command with appropriate local permissions, then rerun setup. An administrator may need to authorize HTTPS certificates or Funnel for the device. See [Tailscale Funnel documentation](https://tailscale.com/kb/1223/funnel).

## Cloudflare Tunnel

Choose `cloudflare` and enter a hostname in a Cloudflare-managed DNS zone, such as `connect.example.com`.

Crow guides `cloudflared tunnel login`; open its authorization URL on your desktop. It creates a named tunnel, records its credentials, adds the DNS route, and installs a separate persistent user service for `cloudflared`. Temporary quick tunnels are not used because the GitHub App needs a stable webhook URL.

This implementation uses Cloudflare's locally managed tunnel workflow. Its browser login creates an account certificate in `~/.cloudflared`, which has broader tunnel/DNS capabilities than an individual tunnel token. Keep that file private. Crow copies the tunnel-specific credential into its private state directory. HTTPS requests pass through Cloudflare; your Codex authentication stays on your worker.

Cloudflare DNS must already manage the selected zone. Crow does not purchase domains or change nameservers. See [Cloudflare's local tunnel guide](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/local-management/create-local-tunnel/).

## Existing HTTPS

Choose `existing`, then enter an HTTPS origin without a path, query, or credentials. Configure your reverse proxy to forward it to `http://127.0.0.1:8787`, including the temporary `/setup/` paths during onboarding. Crow verifies public connectivity after startup.

When the proxy runs on another machine, provide private connectivity to the Crow listener and adjust the binding in the saved configuration before starting. Protect this connection and retain Crow's application authentication. Do not expose an unauthenticated proxy-management interface.

A tunnel does not store missed webhooks. Crow audits failed deliveries hourly and on recovery, and catch-up covers longer outages. A stopped connection service cannot update GitHub status until it returns.

## GitHub App ownership and installation scope

Setup can register an App under your personal account or an organization where you have App-management permission. Public installation scope is the default because one App can then cover repositories across your personal account and organizations. This does not require publishing Crow's source or listing the App on GitHub Marketplace. Anyone can install a public App, but Crow only processes repositories that its operator explicitly enrolls after GitHub authority checks. Other installations do not grant access to your worker.

Choose private scope to limit the App to its owning account. A private personal App cannot also be installed on an organization; a private organization App is limited to that organization. You can change visibility later in the App's GitHub settings.

GitHub documents [manifest registration for personal and organization accounts](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest) and [public/private installation scope](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/making-a-github-app-public-or-private).
