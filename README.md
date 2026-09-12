# Crow

Crow is a self-hosted GitHub pull-request reviewer. After a one-time setup, a
PR opened or updated on GitHub automatically triggers a review. Crow checks out
the commit, runs your authenticated Codex CLI or Claude Code CLI, and posts a
summary plus inline findings back to the PR. There is nothing to run for each
PR.

## How it works

GitHub sends Crow `pull_request` webhooks for `opened`, `reopened`, and
`synchronize`. Crow verifies the signature, queues the event durably, reviews
one PR at a time in an isolated temporary checkout, and posts a GitHub review.
Only findings on added lines become inline comments; the rest stay in the
summary. A commit marker makes retries and restarts idempotent.

## One-time setup (Docker)

You need a server with Docker, a public HTTPS hostname, and a GitHub account
that can create/install an App.

1. Create a private GitHub App (GitHub **Settings → Developer settings → GitHub
   Apps → New GitHub App**).

   - Homepage URL: `https://YOUR_CROW_HOST`
   - Webhook URL: `https://YOUR_CROW_HOST/webhooks/github`
   - Choose a long random webhook secret and keep it private.
   - Repository permissions: **Contents: Read**, **Pull requests: Read and
     write**, **Issues: Read and write**, and **Metadata: Read**.
   - Subscribe to the **Pull request** event, create the App, install it on the
     repositories Crow should watch, and download the App's private key. Note
     the App ID.

   [github-app-manifest.json](./github-app-manifest.json) lists the same fields
   as a reference. It is not an upload endpoint; the GitHub form is the setup
   flow.

2. Put the key and webhook secret on the server (never commit them):

   ```bash
   mkdir -p secrets
   cp downloaded-app-key.pem secrets/github-app.pem
   printf '%s' 'the-webhook-secret-you-chose' > secrets/webhook-secret
   chmod 600 secrets/github-app.pem secrets/webhook-secret
   cp .env.example .env
   ```

   For a systemd install running as `crow`, make the secret files readable by
   that user (for example, `sudo chown crow:crow secrets/github-app.pem
   secrets/webhook-secret` after creating the account).

   Edit `.env` and set `GITHUB_APP_ID`, `CROW_PROVIDER`, and the public-facing
   host details. The example uses the same relative paths for Docker and a
   normal install.

3. Build Crow, then sign in to the selected CLI. Compose keeps the login in a
   named volume, so do this once as the container's `node` user:

   ```bash
   docker compose build
   # Choose one (the login is persisted in the named auth volume):
   docker compose run --rm -it --entrypoint codex crow login
   docker compose run --rm -it --entrypoint claude crow auth login
   ```

   The image installs `@openai/codex` by default. For Claude, set
   `CROW_PROVIDER=claude` and
   `CROW_CLI_PACKAGE=@anthropic-ai/claude-code@^2.1.259` in `.env` before
   `docker compose build`. Compose sets `CLAUDE_CONFIG_DIR` so Claude's
   config and subscription credentials are kept in the mounted auth volume.

4. Start Crow and put an HTTPS reverse proxy (Caddy, nginx, or equivalent) in
   front of its local port:

   ```bash
   docker compose up -d
   curl http://127.0.0.1:8787/health  # replace 8787 if PORT is customized
   docker compose logs -f crow
   ```

   `/health` includes `configured: true/false`; the Compose health check only
   reports healthy once the App key and webhook secret are readable.

   The proxy must forward `/webhooks/github` to Crow. GitHub cannot deliver a
   webhook to a private HTTP-only address.

## Non-Docker install

Use Node `22.12` or newer. On Linux, install `bubblewrap` first so Codex's
read-only sandbox can start (`sudo apt install bubblewrap` on Debian/Ubuntu).
The host must also allow unprivileged user namespaces (or the equivalent
bubblewrap configuration); keep the sandbox enabled rather than bypassing it.
Use a recent Codex CLI that supports `codex exec --output-schema` (check with
`codex exec --help`).
Install the chosen CLI and authenticate it as the same OS user that will run
Crow (a dedicated `crow` user is recommended). If the system Node prefix is
root-owned, install the CLI with `sudo`, then run the login as the account that
will run Crow:

```bash
npm install
sudo npm install --global @openai/codex  # or @anthropic-ai/claude-code@^2.1.259
codex login                               # or: claude auth login
cp .env.example .env
npm start
```

Set the file paths in `.env` to readable local files. For a systemd service,
see [crow.service.example](./crow.service.example). Create the service user's
writable directories before starting it, for example:

```bash
sudo useradd --system --create-home --shell /usr/sbin/nologin crow
sudo install -d -o crow -g crow /opt/crow/.crow-data /home/crow/.codex /home/crow/.claude
```

Run the CLI login as `User=crow` when using systemd (for example,
`sudo -u crow -H codex login`; for Claude use
`sudo -u crow -H env CLAUDE_CONFIG_DIR=/home/crow/.claude claude auth login`.
The administrator's home-directory login is not shared. The example unit sets
`CLAUDE_CONFIG_DIR` so Claude's config and subscription credentials also stay
under `/home/crow/.claude`. If Node or the CLI was installed in a
custom (for example, nvm) prefix, set absolute `CROW_CODEX_BIN`/
`CROW_CLAUDE_BIN` paths and adjust `ExecStart`/`PATH` in the unit.

## Provider choice and safety

Set `CROW_PROVIDER=codex` or `CROW_PROVIDER=claude`. Crow uses the CLI's local
subscription login; it does not call a separate Crow model API. Codex runs with
an ephemeral read-only sandbox and ignores user/project instruction files.
Claude Code 2.1.259 or newer runs in safe/restricted `dontAsk` mode with only the
`Read` tool, no MCP configuration, and no session persistence. The checkout is
temporary, GitHub credentials are removed before the model starts, and
provider output is size-limited and redacted for known API keys. The CLI still
needs to read its own login files, so run Crow in a dedicated user/container
with no unrelated credentials in its home directory. The documented setup uses
the mounted login files; headless token/proxy environment variables are not
forwarded unless you add that integration deliberately.

## Configuration

| Variable | Purpose |
| --- | --- |
| `CROW_PROVIDER` | `codex` or `claude` |
| `CROW_CLI_PACKAGE` | CLI package installed during the Docker build |
| `CROW_FORWARD_API_KEYS` | Set `1` only to pass API-key env vars to the CLI (default `0`; subscription logins do not need this) |
| `CROW_CODEX_BIN` / `CROW_CLAUDE_BIN` | Optional absolute CLI binary path |
| `CLAUDE_CONFIG_DIR` | Optional Claude config/auth directory; Compose points this at its persistent auth volume |
| `GITHUB_APP_ID` | GitHub App ID |
| `GITHUB_PRIVATE_KEY_FILE` | PEM file path (Compose overrides this to its secret mount) |
| `GITHUB_WEBHOOK_SECRET_FILE` | Webhook secret file path (Compose overrides this to its secret mount) |
| `GITHUB_PRIVATE_KEY_PATH` / `GITHUB_WEBHOOK_SECRET_PATH` | Host-side source paths for Compose secrets |
| `GITHUB_API_URL` / `GITHUB_HOST` | Optional GitHub Enterprise API URL (`.../api/v3`) and hostname |
| `CROW_BOT_LOGIN` | Optional exact GitHub App bot login used when checking existing review markers |
| `CROW_STATE_FILE` | Durable queue/history file (default `./.crow-data/state.json`) |
| `CROW_MAX_QUEUE` | Maximum active queued reviews (default `100`) |
| `CROW_MAX_WEBHOOK_BYTES` | Maximum webhook body size (default `2097152`) |
| `CROW_MAX_DIFF_BYTES` | Diff context cap (default `4194304`) |
| `CROW_REVIEW_TIMEOUT_MS` | Per-review CLI timeout (default `180000`) |
| `CROW_MAX_PROVIDER_OUTPUT_BYTES` | CLI output cap (default `8388608`) |
| `PORT` | Local HTTP port (default `8787`) |
| `CROW_BIND_HOST` | Listener address (default `127.0.0.1`; use `0.0.0.0` only with deliberate network controls) |

## Optional status page

The Vite page in this repository is a read-only visual status mock. It is not
needed for automatic reviews and is not served by the webhook worker. Run
`npm run dev` locally or host `npm run build`'s `dist/` separately if you want
to inspect it; GitHub remains the source of truth for review comments.
