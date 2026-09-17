# Linux quickstart

Crow needs an always-on Linux machine, a GitHub account with admin or maintain access to the repositories you enroll, and a Codex subscription. Ubuntu with systemd is the supported guided installation path. Setup can install missing Git, GitHub CLI, Codex, and supported ingress tools with your confirmation.

The [hosted installer](install.md) is live. Run it as your normal user:

```sh
curl -fsSL https://birdapp.dev/install.sh | sh
```

It installs Crow and offers to start setup. No Node installation, package manager, source checkout, or repository download permission is required. Missing system dependencies may need sudo during setup. If you install without setup, continue with `crow setup` or the full command printed by the installer.

1. Choose `both`, the default role. This runs the connection service and worker together.
2. Sign into GitHub CLI if needed. Open its URL on your desktop and enter the device code.
3. Choose public HTTPS. Tailscale Funnel is the default. Crow selects an unused supported port and preserves existing Serve routes. Account authorization may require your browser or a tailnet administrator.
4. Choose whether to create a new GitHub App or [connect an existing App](networking.md#connect-an-existing-github-app). For a new App, open the Crow setup URL on your desktop. Choose whether your personal account or an organization owns the App. The default public installation scope permits your personal and organization repositories; private scope limits installation to the App's owning account. Confirm the App, then install it on the repositories you want to connect. Crow receives the generated App credentials automatically.
5. Authenticate Codex using the URL and device code printed in the terminal. Crow reuses your installed official Codex executable but keeps its settings and subscription state in a separate directory. This one-time login does not replace your usual Codex login.
6. Confirm the provider-reported model and reasoning level. Crow saves explicit selections.
7. Enter the repositories to enroll. Their author policy starts with your GitHub account only; initial open PRs stay excluded.

Setup installs a user systemd service, enables startup after reboot and logout, and checks the connections. It does not create or review a test PR.

```sh
crow status
crow doctor --runtime
```

An installation lives in `~/.local/share/crow`. Set `CROW_HOME` before invoking Crow to use a different directory. Use the same value for setup, administration, and restore. The service records the chosen directory automatically.

If port 8787 is already occupied, choose another local port before onboarding with `crow setup --port 9887`. An established installation needs an explicit route migration to change ports.

A normal authorized PR event starts a review. Request one manually with `crow review owner/repo 123` or an authorized `@crow review` comment. Authorized PR conversation comments can also use `@crow resume`, `@crow restart`, and `@crow pause`. Manual requests still respect draft and author policies.

If setup stops, fix the reported issue and rerun it. Saved App credentials, model selections, and existing repository policies are retained. Re-running setup does not reset an enrolled repository's backlog policy.
