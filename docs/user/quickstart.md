# Linux quickstart

Crow needs an always-on Linux machine, a GitHub account with admin or maintain access to the repositories you enroll, and a Codex subscription. Ubuntu with systemd is the supported guided installation path. Setup can install missing Git, GitHub CLI, Codex, and supported ingress tools with your confirmation.

[Download and verify the Linux binary](install.md). No Node installation, package manager, or source checkout is required.

Run the extracted binary as your normal user:

```sh
./crow setup --role both --ingress funnel
```

Setup copies the executable into Crow's installation, makes it available through `~/.local/bin/crow`, and requests sudo for system changes that need it. Add `~/.local/bin` to your `PATH` if requested. You can remove the downloaded archive and extracted copy after setup completes.

1. Choose `both`, the default role. This runs the connection service and worker together.
2. Sign into GitHub CLI if needed. Open its URL on your desktop and enter the device code.
3. Choose public HTTPS. Tailscale Funnel is the default. Crow selects an unused supported port and preserves existing Serve routes. Account authorization may require your browser or a tailnet administrator.
4. Open the Crow setup URL on your desktop. Choose whether your personal account or an organization owns the App. The default public installation scope permits your personal and organization repositories; private scope limits installation to the App's owning account. Confirm the App, then install it on the repositories you want to connect. Crow receives the generated App credentials automatically.
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

A normal authorized PR event starts a review. Request one manually with `crow review owner/repo 123` or an authorized `@crow review` comment. Manual requests still respect draft and author policies.

If setup stops, fix the reported issue and rerun it. Saved App credentials, model selections, and existing repository policies are retained. Re-running setup does not reset an enrolled repository's backlog policy.
