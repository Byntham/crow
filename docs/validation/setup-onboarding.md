# Setup and default runtime validation

Runtime testing now defaults on in local installation settings. Existing explicit disable settings and repository allowlists are preserved. Missing execution authority in an internal reviewer context still grants no tools, so delegated reviews remain inspection-only.

Normal setup offers to install Podman and rootless helper packages on Ubuntu/Debian with consent for sudo. It rechecks prerequisites, preserves functioning custom runtimes, and offers an explicit inspection-only fallback when the host cannot support execution. Crow's generated systemd unit delegates resource controls to its containers.

The wider setup audit addressed these recovery problems:

- Port conflicts are detected before creating ingress or connecting GitHub. An existing local Crow service must authenticate with this installation's saved credentials before its occupied port is accepted.
- Missing systemd user-session support is reported before account onboarding.
- The installer preserves a custom command directory when starting guided setup.
- Worker setup verifies and reuses saved pairing, and permits replacing a revoked token.
- Model prompts retain only supported saved choices. Doctor validates configured review and subagent models against the catalog it already fetched.
- An outdated Codex binary is detected by its capabilities before login. Setup offers a verified Crow-managed executable without replacing a user-managed binary.

## Checks performed

The complete ordinary Rust suite, formatting, all-target Clippy with warnings denied, and three Python replay tests passed. Focused setup tests simulate package installation, refusal, installation failure, missing helpers, cancellation, custom runtimes and disabled/service-only configurations. Local HTTP fixtures cover occupied ports, existing-service authentication and worker-token verification. A subprocess verifies custom installation directories survive the installer-to-setup handoff. A fake older Codex executable answers its version command but fails the required capability checks, confirming the setup diagnostic path.

The real managed-container scenario now loads normal installation defaults without explicitly enabling execution. It completed 24 concurrent public-package requests during preparation, restored the prepared snapshot and ran a Node check offline. The installation/MCP smoke script also passed with an isolated home and custom command directory.

Package-install decisions were tested with mocks; this pass did not install system packages on the shared development host or create a new GitHub App. The browser account-consent steps and OS administrator changes remain distinct from automated setup validation. Toolchain image provisioning still occurs on first use, and every experiment verifies actual resource limits before repository code runs.
