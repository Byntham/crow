import type { Interface } from "node:readline/promises";
import type { CrowConfig, AppRegistration } from "./types.mjs";
import { createServer } from "node:http";
import { createInterface } from "node:readline/promises";
import { mkdir, readFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { homedir, tmpdir, arch } from "node:os";
import { defaults, save, validateConfig } from "./config.mjs";
import { isBinary, version } from "./runtime.mjs";
import { installDownloaded } from "./install-command.mjs";
import { installCodex } from "./codex-install.mjs";
import {
  atomic,
  isRecord,
  json,
  id,
  equal,
  httpsUrl,
  processRun,
  integer,
} from "./util.mjs";
import {
  installService,
  admin,
  waitForService,
  doctor,
  serviceAction,
} from "./operations.mjs";

type Ask = (label: string, fallback?: string) => Promise<string>;
type Run = typeof processRun;
interface SetupDependencies {
  run?: Run;
  ask?: Ask;
  fetcher?: typeof fetch;
}
interface FunnelStatus {
  TCP?: Record<string, unknown>;
  Web?: Record<string, { Handlers?: Record<string, { Proxy?: string }> }>;
}
function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) throw new Error(`Invalid ${label}`);
  return value;
}
function text(value: unknown, label: string): string {
  if (typeof value !== "string") throw new Error(`Invalid ${label}`);
  return value;
}
function parsedRecord(input: string, label: string): Record<string, unknown> {
  const value: unknown = JSON.parse(input);
  return record(value, label);
}
async function processId(file: string): Promise<number | undefined> {
  const value: unknown = await json(file, null);
  if (value === null) return undefined;
  const pid = record(value, "process record").pid;
  if (pid === undefined) return undefined;
  if (typeof pid !== "number" || !Number.isSafeInteger(pid) || pid <= 0)
    throw new Error("Invalid process ID");
  return pid;
}
function tunnelList(input: string): { id: string; name: string }[] {
  const value: unknown = JSON.parse(input);
  if (!Array.isArray(value)) throw new Error("Invalid Cloudflare tunnel list");
  return value.map((entry: unknown) => {
    const tunnel = record(entry, "Cloudflare tunnel");
    return {
      id: text(tunnel.id, "tunnel ID"),
      name: text(tunnel.name, "tunnel name"),
    };
  });
}
function funnelStatus(value: unknown): FunnelStatus {
  const status = record(value, "Tailscale Serve status");
  const result: FunnelStatus = {};
  if (status.TCP) result.TCP = record(status.TCP, "Tailscale TCP routes");
  if (status.Web) {
    result.Web = {};
    for (const [host, entry] of Object.entries(
      record(status.Web, "Tailscale web routes"),
    )) {
      const route = record(entry, "Tailscale web route");
      const handlers: Record<string, { Proxy?: string }> = {};
      for (const [path, value] of Object.entries(
        record(route.Handlers || {}, "Tailscale handlers"),
      )) {
        const handler = record(value, "Tailscale handler");
        handlers[path] =
          typeof handler.Proxy === "string" ? { Proxy: handler.Proxy } : {};
      }
      result.Web[host] = { Handlers: handlers };
    }
  }
  return result;
}

export async function ensureCommand(
  command: string,
  { run = processRun, ask, fetcher = fetch }: SetupDependencies = {},
) {
  try {
    await run(command, ["--version"]);
    return;
  } catch {}
  if (!ask || process.platform !== "linux")
    throw new Error(`Install ${command}, then rerun crow setup.`);
  if (
    (await ask(
      `${command} is missing. Install the official Linux package using sudo? yes/no`,
      "yes",
    )) !== "yes"
  )
    throw new Error(`Install ${command}, then rerun crow setup.`);
  if (command === "gh" || command === "git") {
    await run("sudo", ["apt-get", "update"], { inherit: true });
    await run("sudo", ["apt-get", "install", "-y", command], { inherit: true });
    return;
  }
  const dir = await mkdtemp(join(tmpdir(), "crow-install-"));
  try {
    if (command === "tailscale") {
      const r = await fetcher("https://tailscale.com/install.sh", {
        signal: AbortSignal.timeout(30000),
      });
      if (!r.ok)
        throw new Error(`Tailscale download returned HTTP ${r.status}`);
      const file = join(dir, "install.sh");
      await writeFile(file, await r.text(), { mode: 0o600 });
      await run("sudo", ["sh", file], { inherit: true });
    } else if (command === "cloudflared") {
      const architecture = arch();
      const cpu =
        architecture === "x64"
          ? "amd64"
          : architecture === "arm64"
            ? "arm64"
            : undefined;
      if (!cpu)
        throw new Error(
          "Install cloudflared manually for this CPU architecture.",
        );
      const r = await fetcher(
        `https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-${cpu}.deb`,
        { signal: AbortSignal.timeout(120000) },
      );
      if (!r.ok)
        throw new Error(`Cloudflare download returned HTTP ${r.status}`);
      const file = join(dir, "cloudflared.deb");
      await writeFile(file, Buffer.from(await r.arrayBuffer()), {
        mode: 0o644,
      });
      await run("sudo", ["dpkg", "-i", file], { inherit: true });
    } else
      throw new Error(`Automatic installation is unavailable for ${command}.`);
    await run(command, ["--version"]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

export function chooseFunnelPort(
  status: FunnelStatus,
  previous?: number,
  target?: string,
) {
  const used = new Set(Object.keys(status.TCP || {}).map(Number));
  for (const address of Object.keys(status.Web || {}))
    used.add(Number(address.split(":").at(-1)));
  // Reuse only the binding recorded by Crow, and only if it still has exactly our route.
  if (previous && [443, 8443, 10000].includes(previous)) {
    if (!used.has(previous)) return previous;
    const routes = Object.entries(status.Web || {}).filter(
      ([host]) => Number(host.split(":").at(-1)) === previous,
    );
    if (routes.length === 1) {
      const handlers = routes[0][1].Handlers || {};
      if (Object.keys(handlers).length === 1 && handlers["/"]?.Proxy === target)
        return previous;
    }
  }
  const port = [8443, 10000, 443].find((p) => !used.has(p));
  if (!port)
    throw new Error(
      "All supported Funnel ports are in use. Free one or choose another HTTPS provider; Crow will not overwrite existing routes.",
    );
  return port;
}
export async function configureFunnel(
  config: CrowConfig,
  {
    run = processRun,
    persistPlan = async () => {},
    ask,
  }: SetupDependencies & {
    persistPlan?: (config: CrowConfig) => Promise<void>;
  } = {},
) {
  const mutate = async (args: string[]) => {
    try {
      return await run("tailscale", args, { inherit: true });
    } catch (error) {
      if (
        !ask ||
        (await ask(
          "Tailscale did not complete the command. If it reported missing system permissions, retry this command with sudo? yes/no",
          "no",
        )) !== "yes"
      )
        throw error;
      return run("sudo", ["tailscale", ...args], { inherit: true });
    }
  };
  let state: Record<string, unknown>;
  try {
    state = parsedRecord(
      (await run("tailscale", ["status", "--json"])).stdout,
      "Tailscale status",
    );
  } catch (e) {
    throw new Error(
      `Tailscale is needed for Funnel. Install it from https://tailscale.com/download/linux, then rerun crow setup. ${e instanceof Error ? e.message : String(e)}`,
    );
  }
  if (state.BackendState !== "Running") {
    await mutate(["up"]);
    state = parsedRecord(
      (await run("tailscale", ["status", "--json"])).stdout,
      "Tailscale status",
    );
  }
  const dnsValue = state.Self
    ? record(state.Self, "Tailscale self").DNSName
    : undefined;
  const dns =
    typeof dnsValue === "string" ? dnsValue.replace(/\.$/, "") : undefined;
  if (!dns)
    throw new Error(
      "Tailscale did not report this host’s DNS name. Enable MagicDNS/HTTPS and retry.",
    );
  const status = funnelStatus(
    JSON.parse((await run("tailscale", ["serve", "status", "--json"])).stdout),
  );
  const target = `http://127.0.0.1:${config.port}`,
    port = chooseFunnelPort(status, config.ingress?.port, target);
  const publicUrl = `https://${dns}${port === 443 ? "" : `:${port}`}`;
  if (
    config.ingress?.pending &&
    (config.ingress.port !== port || config.publicUrl !== publicUrl)
  )
    throw new Error(
      "The pending Crow Funnel route has changed. Restore its recorded hostname and dedicated port before resuming setup.",
    );
  config.ingress = { type: "funnel", port, target, pending: true };
  config.publicUrl = publicUrl;
  await persistPlan(config);
  await mutate(["funnel", "--bg", `--https=${port}`, target]);
  delete config.ingress.pending;
  await persistPlan(config);
  return config;
}
async function configureCloudflare(
  config: CrowConfig,
  root: string,
  hostname: string,
  { run = processRun }: SetupDependencies = {},
) {
  hostname = new URL(httpsUrl(`https://${hostname}`)).hostname;
  try {
    await run("cloudflared", ["--version"]);
  } catch {
    throw new Error(
      "Install cloudflared from https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/ and rerun setup.",
    );
  }
  const cf = join(homedir(), ".cloudflared");
  if (!(await json(join(root, "cloudflare-created.json"), null))) {
    try {
      await readFile(join(cf, "cert.pem"));
    } catch {
      await run("cloudflared", ["tunnel", "login"], { inherit: true });
    }
    const name = `crow-${config.worker.id.slice(0, 12)}`;
    const all = tunnelList(
      (await run("cloudflared", ["tunnel", "list", "--output", "json"])).stdout,
    );
    let tunnel = all.find((t) => t.name === name);
    if (!tunnel) {
      await run("cloudflared", ["tunnel", "create", name], { inherit: true });
      tunnel = tunnelList(
        (await run("cloudflared", ["tunnel", "list", "--output", "json"]))
          .stdout,
      ).find((t) => t.name === name);
    }
    if (!tunnel?.id)
      throw new Error("Cloudflare did not return the named tunnel ID.");
    await atomic(join(root, "cloudflare-created.json"), {
      id: tunnel.id,
      name,
    });
  }
  const savedTunnel = record(
    await json(join(root, "cloudflare-created.json")),
    "saved Cloudflare tunnel",
  );
  const tunnel = { id: text(savedTunnel.id, "saved tunnel ID") };
  const credentials = await readFile(join(cf, `${tunnel.id}.json`), "utf8");
  await atomic(join(root, "cloudflare-credentials.json"), credentials);
  await run("cloudflared", ["tunnel", "route", "dns", tunnel.id, hostname], {
    inherit: true,
  });
  const file = join(root, "cloudflare.json");
  await atomic(file, {
    tunnel: tunnel.id,
    "credentials-file": join(root, "cloudflare-credentials.json"),
    ingress: [
      { hostname, service: `http://127.0.0.1:${config.port}` },
      { service: "http_status:404" },
    ],
  });
  config.ingress = { type: "cloudflare", file, tunnel: tunnel.id };
  config.publicUrl = `https://${hostname}`;
  return config;
}
const htmlEscape = (s: unknown) =>
  String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
export function appManifest(config: CrowConfig, name: string) {
  return {
    name,
    url: config.publicUrl,
    hook_attributes: {
      url: `${config.publicUrl}/webhooks/github`,
      active: true,
    },
    redirect_url: `${config.publicUrl}/setup/callback`,
    public: config.appRegistration?.visibility !== "private",
    default_permissions: {
      contents: "read",
      metadata: "read",
      pull_requests: "write",
      issues: "write",
    },
    default_events: [
      "pull_request",
      "push",
      "issue_comment",
      "installation",
      "installation_repositories",
    ],
  };
}
export function registrationAction(
  registration: AppRegistration,
  state: string,
) {
  if (!/^[a-f0-9]{64}$/.test(state))
    throw new Error("Invalid registration state");
  if (registration.ownerType === "personal")
    return `https://github.com/settings/apps/new?state=${state}`;
  if (
    registration.ownerType !== "organization" ||
    !registration.organization ||
    !/^[-A-Za-z0-9]{1,39}$/.test(registration.organization) ||
    registration.organization.startsWith("-") ||
    registration.organization.endsWith("-")
  )
    throw new Error("Invalid GitHub organization name");
  return `https://github.com/organizations/${encodeURIComponent(registration.organization)}/settings/apps/new?state=${state}`;
}
export async function registerApp(
  config: CrowConfig,
  root: string,
  {
    ask,
    log = console.log,
    fetcher = fetch,
    timeoutMs = 20 * 60 * 1000,
  }: SetupDependencies & {
    log?: (value: unknown) => void;
    timeoutMs?: number;
  } = {},
) {
  if (!ask) throw new Error("GitHub registration requires interactive prompts");
  const state = id() + id(),
    secret = id() + id();
  const {
    promise: completed,
    resolve: finish,
    reject: fail,
  } = Promise.withResolvers<void>();
  let busy = false;
  const previous = config.appRegistration || {
    ownerType: "personal",
    visibility: "public",
  };
  const ownerType = await ask(
    "GitHub App owner: personal or organization",
    previous.ownerType,
  );
  if (ownerType !== "personal" && ownerType !== "organization")
    throw new Error("Choose personal or organization App ownership");
  const organization =
    ownerType === "organization"
      ? await ask("GitHub organization login", previous.organization || "")
      : undefined;
  log(
    "A public App can be installed on your personal and organization accounts. No Marketplace listing is created; only repositories you explicitly enroll can use Crow. A private App is limited to its owning account.",
  );
  const visibility = await ask(
    "App installation scope: public or private",
    previous.visibility,
  );
  if (visibility !== "public" && visibility !== "private")
    throw new Error("Choose public or private App visibility");
  const registration: AppRegistration = { ownerType, organization, visibility };
  const action = registrationAction(registration, state);
  config.appRegistration = registration;
  await save(config, root);
  const manifest = appManifest(
    config,
    await ask(
      "GitHub App name",
      `Crow ${config.operator} ${config.worker.id.slice(0, 6)}`,
    ),
  );
  const server = createServer(async (req, res) => {
    const end = (status: number, body: string, type = "text/plain") => {
      res.writeHead(status, {
        "Content-Type": type,
        "Cache-Control": "no-store",
        "Referrer-Policy": "no-referrer",
        "X-Content-Type-Options": "nosniff",
      });
      res.end(body);
    };
    try {
      const u = new URL(req.url || "/", "http://localhost");
      if (req.method === "GET" && u.pathname === "/health")
        return end(
          200,
          JSON.stringify({ ok: true, setup: true }),
          "application/json",
        );
      if (req.method === "GET" && equal(u.pathname, `/setup/${secret}`))
        return end(
          200,
          `<!doctype html><meta charset="utf-8"><title>Set up Crow</title><h1>Create your Crow GitHub App</h1><p>GitHub will ask you to confirm the App. Then return here to install it.</p><form action="${htmlEscape(action)}" method="post"><input type="hidden" name="manifest" value="${htmlEscape(JSON.stringify(manifest))}"><button>Create GitHub App</button></form>`,
          "text/html",
        );
      if (req.method === "GET" && u.pathname === "/setup/callback") {
        const code = u.searchParams.get("code");
        if (!equal(u.searchParams.get("state"), state) || !code || busy)
          return end(
            400,
            "Invalid or already used setup callback. Return to your terminal.",
          );
        busy = true;
        const response = await fetcher(
          `https://api.github.com/app-manifests/${encodeURIComponent(code)}/conversions`,
          {
            method: "POST",
            headers: {
              Accept: "application/vnd.github+json",
              "User-Agent": "Crow",
            },
            signal: AbortSignal.timeout(30000),
          },
        );
        if (!response.ok)
          throw new Error(
            `GitHub App registration failed: HTTP ${response.status}`,
          );
        const app = record(await response.json(), "GitHub App credentials");
        if (
          (typeof app.id !== "number" && typeof app.id !== "string") ||
          !app.id ||
          typeof app.pem !== "string" ||
          !app.pem ||
          typeof app.webhook_secret !== "string" ||
          !app.webhook_secret ||
          typeof app.slug !== "string" ||
          !app.slug
        )
          throw new Error("GitHub returned incomplete App credentials.");
        config.app = {
          id: app.id,
          pem: app.pem,
          webhookSecret: app.webhook_secret,
          slug: app.slug,
          botId: null,
        };
        await save(config, root);
        end(
          200,
          `<!doctype html><meta charset="utf-8"><h1>App created</h1><p><a href="https://github.com/apps/${encodeURIComponent(app.slug)}/installations/new">Install Crow and select repositories</a>, then return to your terminal.</p>`,
          "text/html",
        );
        finish();
        return;
      }
      end(404, "Not found");
    } catch (e) {
      end(500, "Setup failed. See the Crow terminal for details.");
      fail(e);
    }
  });
  await new Promise<void>((r, j) => {
    server.once("error", j);
    server.listen(config.port, config.bind, r);
  });
  const timer = setTimeout(
    () =>
      fail(new Error("GitHub setup timed out. Run crow setup to continue.")),
    timeoutMs,
  );
  try {
    log(`Open this URL on your desktop: ${config.publicUrl}/setup/${secret}`);
    await completed;
  } finally {
    clearTimeout(timer);
    await new Promise<void>((r) => server.close(() => r()));
  }
}
export async function githubIdentity({
  run = processRun,
  interactive = false,
}: { run?: Run; interactive?: boolean } = {}) {
  try {
    await run("gh", ["--version"]);
  } catch {
    throw new Error(
      "Install GitHub CLI from https://cli.github.com, then rerun crow setup.",
    );
  }
  try {
    await run("gh", ["auth", "status", "--hostname", "github.com"]);
  } catch (e) {
    if (!interactive)
      throw new Error("Run gh auth login --hostname github.com --web first.");
    await run(
      "gh",
      [
        "auth",
        "login",
        "--hostname",
        "github.com",
        "--web",
        "--git-protocol",
        "https",
      ],
      { inherit: true },
    );
  }
  const user = parsedRecord(
    (await run("gh", ["api", "user"])).stdout,
    "GitHub identity",
  );
  const token = (
    await run("gh", ["auth", "token", "--hostname", "github.com"])
  ).stdout.trim();
  return { login: text(user.login, "GitHub login"), token };
}
export function applySetupPort(config: CrowConfig, port: unknown) {
  if (port === undefined) return;
  if (!["string", "number"].includes(typeof port))
    throw new Error("Provide a numeric setup port");
  const selected = integer(Number(port), 1, 65535, "setup port");
  if (config.role === "worker")
    throw new Error("Worker-only setup has no listening port.");
  if (selected !== config.port && (config.app || config.publicUrl))
    throw new Error(
      "Choose --port before HTTPS or GitHub App onboarding. An existing installation needs an explicit route migration.",
    );
  config.port = selected;
  config.serviceUrl = `http://127.0.0.1:${selected}`;
}
export async function setup(
  root: string,
  {
    role,
    ingress,
    port,
    ask: providedAsk,
    log = console.log,
    run = processRun,
  }: {
    role?: string;
    ingress?: string;
    port?: unknown;
    ask?: Ask;
    log?: (value: unknown) => void;
    run?: Run;
  } = {},
) {
  let rl: Interface | undefined;
  const ask: Ask =
    providedAsk ||
    (async (label, fallback = "") => {
      rl ??= createInterface({ input: process.stdin, output: process.stdout });
      const answer = (
        await rl.question(
          `${label}${fallback !== "" ? ` [${fallback}]` : ""}: `,
        )
      ).trim();
      return answer || fallback;
    });
  try {
    await mkdir(root, { recursive: true, mode: 0o700 });
    if (isBinary) {
      const executable = await installDownloaded(root, { version: version() });
      log(`Installed Crow at ${executable}`);
      if (
        !(process.env.PATH || "")
          .split(":")
          .includes(join(homedir(), ".local/bin"))
      )
        log(
          'Add ~/.local/bin to your PATH. For this shell, run: export PATH="$HOME/.local/bin:$PATH"',
        );
    }
    const stored: unknown = await json(join(root, "config.json"), null);
    const config = stored ? validateConfig(stored) : defaults(root);
    const selectedRole =
      role || (await ask("Role: both, service, or worker", config.role));
    if (
      selectedRole !== "both" &&
      selectedRole !== "service" &&
      selectedRole !== "worker"
    )
      throw new Error("Choose both, service, or worker");
    config.role = selectedRole;
    applySetupPort(config, port);
    await save(config, root);
    let identity: { login: string; token: string } | undefined;
    if (config.role === "worker") {
      config.serviceUrl = httpsUrl(
        await ask(
          "Connection-service HTTPS URL",
          config.serviceUrl.startsWith("https:") ? config.serviceUrl : "",
        ),
      );
      config.worker.id = await ask(
        "Worker ID from crow pair",
        config.worker.id,
      );
      config.worker.token = await ask("Worker token from crow pair");
      if (!config.worker.token)
        throw new Error("Worker pairing token required");
      await save(config, root);
    } else {
      await ensureCommand("gh", { run, ask });
      identity = await githubIdentity({ run, interactive: true });
      if (
        config.operator &&
        config.operator.toLowerCase() !== identity.login.toLowerCase()
      )
        throw new Error(
          `GitHub CLI is signed in as ${identity.login}; this installation belongs to ${config.operator}.`,
        );
      config.operator = identity.login;
      await save(config, root);
      if (!config.publicUrl || config.ingress?.pending) {
        const type =
          (config.ingress?.pending ? config.ingress.type : ingress) ||
          (await ask(
            "Public HTTPS: funnel, cloudflare, or existing",
            config.ingress.type,
          ));
        if (type === "funnel") {
          await ensureCommand("tailscale", { run, ask });
          await configureFunnel(config, {
            run,
            ask,
            persistPlan: () => save(config, root),
          });
        } else if (type === "cloudflare") {
          await ensureCommand("cloudflared", { run, ask });
          await configureCloudflare(
            config,
            root,
            await ask("Cloudflare hostname, for example connect.example.com"),
            { run, ask },
          );
        } else if (type === "existing") {
          config.publicUrl = httpsUrl(await ask("Public HTTPS origin"));
          config.ingress = { type: "existing" };
        } else throw new Error("Choose funnel, cloudflare, or existing");
        await save(config, root);
      }
      if (config.ingress.type === "cloudflare") {
        log("Cloudflare tunnel startup is required for the browser callback.");
        await installTunnelService(config, { run });
      }
      if (!config.app) await registerApp(config, root, { ask, log });
      if (!config.app)
        throw new Error("GitHub App registration did not complete");
      log(
        `Install/select repositories on your desktop: https://github.com/apps/${config.app.slug}/installations/new`,
      );
      await ask("Press Enter when the App is installed");
      const { GitHub } = await import("./github.mjs");
      config.app.botId = await new GitHub(config.app).botId();
      await save(config, root);
    }
    if (config.role !== "service") {
      await ensureCommand("git", { run, ask });
      const provider = await import("./provider.mjs");
      try {
        await run(config.worker.codex, ["--version"]);
      } catch {
        if (
          (await ask(
            "Codex is missing. Install the latest official standalone Codex package? yes/no",
            "yes",
          )) !== "yes"
        )
          throw new Error("Install Codex and rerun setup.");
        config.worker.codex = await installCodex(root, { run });
        await save(config, root);
      }
      if (!(await provider.authStatus(config.worker, root)).authenticated) {
        log(
          "Authenticate on your desktop using the URL and code printed below.",
        );
        await provider.login(config.worker, root);
      }
      const catalog = await provider.discover(config.worker, root);
      if (catalog.warning) log(catalog.warning);
      const initial =
        catalog.models.find((m) => m.isDefault) ||
        catalog.models.find((m) =>
          [m.id, m.model].some((id) => id === config.worker.model),
        );
      if (!initial)
        throw new Error(
          "The provider did not report a default model. Retry model discovery before completing initial setup.",
        );
      log(
        catalog.models
          .map((m) => `${m.model || m.id}: ${m.displayName || m.model || m.id}`)
          .join("\n"),
      );
      config.worker.model = await ask(
        "Review model",
        config.worker.model || initial.model || initial.id || "",
      );
      const selected = catalog.models.find((m) =>
        [m.model, m.id].some((id) => id === config.worker.model),
      );
      if (!selected) throw new Error("Select a model from the provider list.");
      config.worker.effort = await ask(
        `Reasoning level (${selected.supportedReasoningEfforts.map((e) => e.reasoningEffort).join(", ")})`,
        config.worker.effort || selected.defaultReasoningEffort,
      );
      if (
        !selected.supportedReasoningEfforts.some(
          (e) => e.reasoningEffort === config.worker.effort,
        )
      )
        throw new Error("Unsupported reasoning level for this model.");
      await save(config, root);
    }
    if (config.role !== "worker") {
      let running = false;
      try {
        await admin(config, "status");
        running = true;
      } catch {}
      if (running) {
        log("Waiting for active reviews before applying setup changes.");
        await admin(config, "drain", {});
        for (;;) {
          const state = await admin(config, "status");
          if (
            !(state.jobs || []).some((j) =>
              ["reviewing", "publishing"].includes(j.state),
            )
          )
            break;
          await new Promise((r) => setTimeout(r, 1000));
        }
        await serviceAction(root, "stop", { run });
      }
    } else {
      const pid = await processId(join(root, "runtime.lock"));
      if (pid) {
        await rm(join(root, "drained.json"), { force: true });
        process.kill(pid, "SIGUSR1");
        for (;;) {
          const drainedPid = await processId(join(root, "drained.json"));
          if (drainedPid === pid) break;
          try {
            process.kill(pid, 0);
          } catch {
            break;
          }
          await new Promise((r) => setTimeout(r, 1000));
        }
        await serviceAction(root, "stop", { run });
      }
    }
    await installService(root, { run });
    if (config.role !== "worker") {
      await waitForService(config);
      await admin(config, "undrain", {});
      const current = await admin(config, "status");
      if (config.role === "service")
        log(
          "Pair a worker with crow pair, then enroll repositories using crow enroll owner/repo --worker ID.",
        );
      const names =
        config.role === "service"
          ? ""
          : await ask(
              "Repositories to enroll, owner/name separated by commas; blank to keep current selection",
            );
      for (const name of names
        .split(",")
        .map((x) => x.trim())
        .filter(Boolean)
        .filter(
          (name) =>
            !current.repos.some(
              (r) => r.name.toLowerCase() === name.toLowerCase(),
            ),
        ))
        log(
          await admin(config, "enroll", {
            repo: name,
            githubToken: identity?.token,
            worker: config.worker.id,
            policy: "selected",
            authors: [config.operator],
            includeBacklog: false,
          }),
        );
    }
    const result = await doctor(config, root, { runtime: true });
    log(JSON.stringify(result, null, 2));
    if (!result.ok)
      throw new Error(
        "Some setup checks failed. Fix the reported issue and rerun crow setup.",
      );
    log(
      "Crow is running persistently. Setup did not start a test review. Use crow status to see activity.",
    );
    return config;
  } finally {
    rl?.close();
  }
}
async function installTunnelService(
  config: CrowConfig,
  {
    run = processRun,
    base = join(homedir(), ".config/systemd/user"),
  }: { run?: Run; base?: string } = {},
) {
  if (config.ingress.type !== "cloudflare") return;
  if (!config.ingress.file)
    throw new Error("Cloudflare tunnel configuration file is missing");
  // JSON is valid YAML; arguments are quoted for systemd rather than a shell.
  const escaped = (value: string) =>
    '"' +
    value
      .replaceAll("\\", "\\\\")
      .replaceAll('"', '\\"')
      .replaceAll("%", "%%") +
    '"';
  const executable = (await run("which", ["cloudflared"])).stdout.trim();
  if (!executable.startsWith("/")) throw new Error("Cannot locate cloudflared");
  const name = `crow-tunnel-${config.worker.id.slice(0, 12)}.service`;
  await atomic(
    join(base, name),
    `[Unit]\nDescription=Crow Cloudflare Tunnel\nAfter=network-online.target\n\n[Service]\nExecStart=${escaped(executable)} tunnel --config ${escaped(config.ingress.file)} run\nRestart=on-failure\nRestartSec=5\nUMask=0077\n\n[Install]\nWantedBy=default.target\n`,
  );
  await run("systemctl", ["--user", "daemon-reload"]);
  await run("systemctl", ["--user", "enable", "--now", name]);
}
