import { homedir, userInfo } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { mkdir, rm } from "node:fs/promises";
import {
  atomic,
  processRun,
  cleanEnv,
  json,
  hash,
  isRecord,
  errorMessage,
} from "./util.mjs";
import type { CrowConfig } from "./types.mjs";

import type { ProcessRunner as Run } from "./util.mjs";
interface UnitOptions {
  executable?: string;
  cli?: string;
  path?: string;
}
interface InstallMetadata {
  installation?: string;
  source?: string;
  bin?: string;
  revision?: string | null;
}
export interface AdminStatus extends Record<string, unknown> {
  jobs: { state: string }[];
  repos: { name: string }[];
}
interface UpdateStatus {
  checkedAt: number;
  available: boolean | null;
  commits?: number;
  warning?: string;
  cached?: boolean;
}
function object(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) throw new Error("Expected an object");
  return value;
}
async function metadataAt(release: string): Promise<InstallMetadata | null> {
  const value: unknown = await json(join(release, "install.json"), null);
  if (value === null) return null;
  const entry = object(value);
  for (const key of ["installation", "source", "bin"]) {
    if (entry[key] !== undefined && typeof entry[key] !== "string")
      throw new Error("Invalid Crow install metadata");
  }
  if (
    entry.revision !== undefined &&
    entry.revision !== null &&
    typeof entry.revision !== "string"
  )
    throw new Error("Invalid Crow install metadata");
  return {
    installation:
      typeof entry.installation === "string" ? entry.installation : undefined,
    source: typeof entry.source === "string" ? entry.source : undefined,
    bin: typeof entry.bin === "string" ? entry.bin : undefined,
    revision:
      typeof entry.revision === "string" || entry.revision === null
        ? entry.revision
        : undefined,
  };
}
// Installed releases have metadata and flattened bin/lib directories. Development output lives in dist.
const sourceRoot = (release: string, metadata: InstallMetadata | null) =>
  metadata?.source ||
  (!metadata && basename(release) === "dist" ? dirname(release) : release);
async function processId(file: string): Promise<number | undefined> {
  const value: unknown = await json(file, null);
  if (value === null) return undefined;
  const pid = object(value).pid;
  if (pid === undefined) return undefined;
  if (typeof pid !== "number" || !Number.isSafeInteger(pid) || pid <= 0)
    throw new Error("Invalid Crow process identifier");
  return pid;
}
function statusResult(value: unknown): AdminStatus {
  const result = object(value);
  if (!Array.isArray(result.jobs) || !Array.isArray(result.repos))
    throw new Error("Invalid Crow status response");
  const jobs = result.jobs.map((value: unknown) => {
    const job = object(value);
    if (typeof job.state !== "string")
      throw new Error("Invalid review state in Crow status");
    return { ...job, state: job.state };
  });
  const repos = result.repos.map((value: unknown) => {
    const repo = object(value);
    if (typeof repo.name !== "string")
      throw new Error("Invalid repository in Crow status");
    return { ...repo, name: repo.name };
  });
  return { ...result, jobs, repos };
}
export const cliPath = fileURLToPath(
  new URL("../bin/crow.mjs", import.meta.url),
);
export const unitName = (root: string) =>
  `crow-${hash(resolve(root)).slice(0, 16)}.service`;
const quote = (value: unknown) =>
  '"' +
  String(value)
    .replaceAll("\\", "\\\\")
    .replaceAll('"', '\\"')
    .replaceAll("%", "%%")
    .replaceAll("$", "$$")
    .replaceAll("\n", "\\n") +
  '"';
export function unitText(
  root: string,
  {
    executable = process.execPath,
    cli = cliPath,
    path = process.env.PATH,
  }: UnitOptions = {},
) {
  return `[Unit]\nDescription=Crow PR review service\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart=${quote(executable)} ${quote(cli)} run\nEnvironment=${quote(`CROW_HOME=${resolve(root)}`)}\nEnvironment=${quote(`PATH=${dirname(executable)}:${path || "/usr/local/bin:/usr/bin:/bin"}`)}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=45\nUMask=0077\n\n[Install]\nWantedBy=default.target\n`;
}
export async function installService(
  root: string,
  {
    run = processRun,
    base = join(homedir(), ".config/systemd/user"),
  }: { run?: Run; base?: string } = {},
) {
  if (process.platform !== "linux")
    throw new Error(
      "Persistent startup currently requires Linux and systemd. Use crow run in your service manager.",
    );
  const metadata = await metadataAt(dirname(dirname(cliPath)));
  const cli = metadata?.installation
    ? join(metadata.installation, "current/bin/crow.mjs")
    : cliPath;
  await mkdir(base, { recursive: true });
  await atomic(join(base, unitName(root)), unitText(root, { cli }));
  await run("systemctl", ["--user", "daemon-reload"]);
  const { stdout } = await run("loginctl", [
    "show-user",
    userInfo().username,
    "--property=Linger",
    "--value",
  ]);
  if (stdout.trim() !== "yes") {
    try {
      await run("loginctl", ["enable-linger", userInfo().username], {
        inherit: true,
      });
    } catch {
      await run("sudo", ["loginctl", "enable-linger", userInfo().username], {
        inherit: true,
      });
    }
  }
  await run("systemctl", ["--user", "enable", "--now", unitName(root)]);
}
export async function serviceAction(
  root: string,
  action: string,
  { run = processRun }: { run?: Run } = {},
) {
  if (!["start", "stop", "restart", "status"].includes(action))
    throw new Error("Invalid service action");
  return run("systemctl", ["--user", action, unitName(root)], {
    inherit: true,
  });
}
export function admin(
  config: CrowConfig,
  action: "status",
  body?: undefined,
  options?: { fetcher?: typeof fetch },
): Promise<AdminStatus>;
export function admin(
  config: CrowConfig,
  action: string,
  body?: unknown,
  options?: { fetcher?: typeof fetch },
): Promise<unknown>;
export async function admin(
  config: CrowConfig,
  action: string,
  body?: unknown,
  { fetcher = fetch }: { fetcher?: typeof fetch } = {},
): Promise<unknown> {
  if (config.role === "worker")
    throw new Error("Run this command on the connection-service host.");
  const base = config.serviceUrl || `http://127.0.0.1:${config.port}`;
  const response = await fetcher(new URL(`/admin/${action}`, base), {
    method: body === undefined ? "GET" : "POST",
    headers: {
      Authorization: `Bearer ${config.adminToken}`,
      "Content-Type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(30000),
  });
  const result: unknown = await response.json();
  if (!response.ok)
    throw new Error(
      typeof object(result).error === "string"
        ? String(object(result).error)
        : `Crow returned HTTP ${response.status}`,
    );
  return action === "status" ? statusResult(result) : result;
}
export async function waitForService(
  config: CrowConfig,
  { fetcher = fetch }: { fetcher?: typeof fetch } = {},
) {
  for (let i = 0; i < 30; i++) {
    try {
      return await admin(config, "status", undefined, { fetcher });
    } catch (e) {
      if (i === 29) throw e;
      await new Promise((r) => setTimeout(r, 1000));
    }
  }
}
export async function doctor(
  config: CrowConfig,
  root: string,
  {
    runtime = false,
    fetcher = fetch,
  }: { runtime?: boolean; fetcher?: typeof fetch } = {},
) {
  const checks: { name: string; ok: boolean; detail: unknown }[] = [];
  const check = async (name: string, fn: () => Promise<unknown>) => {
    try {
      checks.push({ name, ok: true, detail: await fn() });
    } catch (e) {
      checks.push({ name, ok: false, detail: errorMessage(e) });
    }
  };
  if (config.role !== "worker") {
    await check("local connection service", () =>
      admin(config, "status", undefined, { fetcher }),
    );
    await check("public HTTPS", async () => {
      if (!config.publicUrl) throw new Error("No public URL configured");
      const r = await fetcher(new URL("/health", config.publicUrl), {
        signal: AbortSignal.timeout(15000),
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const body = object(await r.json());
      if (body.service !== "crow" || !body.configured)
        throw new Error("HTTPS does not reach this configured Crow service");
      return body;
    });
    await check("GitHub App", async () => {
      const { GitHub, jwt } = await import("./github.mjs");
      if (!config.app) throw new Error("GitHub App registration incomplete");
      const app = object(
        await new GitHub(config.app).request("/app", {
          token: jwt(config.app),
        }),
      );
      const permissions = app.permissions ? object(app.permissions) : {};
      if (
        permissions.contents !== "read" ||
        permissions.pull_requests !== "write" ||
        permissions.issues !== "write"
      )
        throw new Error(
          "GitHub App permissions do not match Crow requirements",
        );
      return app.slug;
    });
  } else
    await check("connection service reachable", async () => {
      const r = await fetcher(new URL("/health", config.serviceUrl), {
        signal: AbortSignal.timeout(15000),
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const body = object(await r.json());
      if (body.service !== "crow" || !body.configured)
        throw new Error("HTTPS does not reach this configured Crow service");
      return body;
    });
  if (config.role !== "service") {
    await check("worker pairing", async () => {
      const r = await fetcher(new URL("/worker/ping", config.serviceUrl), {
        method: "POST",
        headers: {
          Authorization: `Bearer ${config.worker.token}`,
          "Content-Type": "application/json",
        },
        body: "{}",
        signal: AbortSignal.timeout(15000),
      });
      if (!r.ok) throw new Error(`Worker pairing returned HTTP ${r.status}`);
      const body = object(await r.json());
      if (body.id !== config.worker.id)
        throw new Error("Pairing belongs to another worker");
      return body.id;
    });
    const p = await import("./provider.mjs");
    await check("subscription authentication", async () => {
      const s = await p.authStatus(config.worker, root);
      if (!s.authenticated) throw new Error(s.warning || "Run crow login");
      return s.account;
    });
    if (runtime)
      await check("review runtime", () =>
        p.diagnostics(config.worker, root).then((r) => {
          if (!r.ok) throw new Error(JSON.stringify(r));
          return r;
        }),
      );
  }
  await check("persistent startup", async () => {
    const r = await processRun("systemctl", [
      "--user",
      "is-enabled",
      unitName(root),
    ]);
    const active = await processRun("systemctl", [
      "--user",
      "is-active",
      unitName(root),
    ]);
    if (active.stdout.trim() !== "active")
      throw new Error("Crow service is not active");
    return `${r.stdout.trim()}, ${active.stdout.trim()}`;
  });
  return { ok: checks.every((x) => x.ok), checks };
}
export async function update(
  root: string,
  config: CrowConfig,
  { run = processRun }: { run?: Run } = {},
) {
  const metadata = await metadataAt(dirname(dirname(cliPath)));
  const project = sourceRoot(dirname(dirname(cliPath)), metadata);
  const status = await run("git", ["status", "--porcelain"], { cwd: project });
  if (status.stdout.trim())
    throw new Error(
      "Crow checkout has local changes. Commit or move them before crow update.",
    );
  await run("git", ["fetch", "--quiet"], { cwd: project });
  const ref = await run(
    "git",
    ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    { cwd: project },
  );
  const count = await run(
    "git",
    ["rev-list", "--count", `HEAD..${ref.stdout.trim()}`],
    { cwd: project },
  );
  if (Number(count.stdout.trim()) === 0)
    return { updated: false, message: "Crow is current." };
  if (config.role !== "worker") await admin(config, "drain", {});
  else {
    const pid = await processId(join(root, "runtime.lock"));
    if (pid) {
      await rm(join(root, "drained.json"), { force: true });
      process.kill(pid, "SIGUSR1");
      for (;;) {
        const drained = await processId(join(root, "drained.json"));
        if (drained === pid) break;
        try {
          process.kill(pid, 0);
        } catch {
          throw new Error("Worker exited while draining");
        }
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
  }
  try {
    if (config.role !== "worker")
      for (;;) {
        const s = await admin(config, "status");
        if (
          !(s.jobs || []).some((j) =>
            ["reviewing", "publishing"].includes(j.state),
          )
        )
          break;
        await new Promise((r) => setTimeout(r, 1000));
      }
    await serviceAction(root, "stop", { run });
    await run("git", ["merge", "--ff-only", ref.stdout.trim()], {
      cwd: project,
    });
    await run(process.execPath, ["scripts/check.mjs", "--runtime-only"], {
      cwd: project,
      inherit: true,
    });
    if (metadata?.installation) {
      await run("bash", ["scripts/install.sh"], {
        cwd: project,
        env: cleanEnv({
          CROW_INSTALL_DIR: metadata.installation,
          CROW_BIN_DIR: metadata.bin || join(homedir(), ".local/bin"),
          CROW_NODE: process.execPath,
        }),
        inherit: true,
      });
      await installService(root, { run });
    } else await serviceAction(root, "start", { run });
    return { updated: true };
  } catch (e) {
    throw new Error(
      `Update stopped: ${errorMessage(e)}. Inspect the checkout, then run crow start.`,
    );
  } finally {
    if (config.role !== "worker")
      await admin(config, "undrain", {}).catch(() => {});
  }
}

export async function updateAvailability(
  root: string,
  {
    run = processRun,
    now = Date.now(),
    release = dirname(dirname(cliPath)),
  }: { run?: Run; now?: number; release?: string } = {},
) {
  const cache = join(root, "update-status.json");
  const cachedValue: unknown = await json(cache, null);
  let previous: UpdateStatus | null = null;
  if (cachedValue !== null) {
    const cached = object(cachedValue);
    if (
      typeof cached.checkedAt === "number" &&
      (typeof cached.available === "boolean" || cached.available === null)
    ) {
      previous = {
        checkedAt: cached.checkedAt,
        available: cached.available,
        ...(typeof cached.commits === "number"
          ? { commits: cached.commits }
          : {}),
        ...(typeof cached.warning === "string"
          ? { warning: cached.warning }
          : {}),
      };
    }
  }
  if (previous?.checkedAt && now - previous.checkedAt < 86400000)
    return { ...previous, cached: true };
  try {
    const signal = AbortSignal.timeout(10000);
    const metadata = await metadataAt(release);
    const project = sourceRoot(release, metadata);
    await run("git", ["fetch", "--quiet"], {
      cwd: project,
      timeout: 10000,
      signal,
    });
    const upstream = (
      await run(
        "git",
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        { cwd: project, timeout: 5000, signal },
      )
    ).stdout.trim();
    if (!upstream || upstream.startsWith("-"))
      throw new Error("No usable upstream branch is configured");
    const revision = metadata?.revision;
    const installed =
      revision && /^[a-f0-9]{40,64}$/.test(revision) ? revision : "HEAD";
    const count = Number(
      (
        await run("git", ["rev-list", "--count", `${installed}..${upstream}`], {
          cwd: project,
          timeout: 5000,
          signal,
        })
      ).stdout.trim(),
    );
    if (!Number.isSafeInteger(count) || count < 0)
      throw new Error("Invalid update comparison");
    const result = { checkedAt: now, available: count > 0, commits: count };
    await atomic(cache, result);
    return { ...result, cached: false };
  } catch (error) {
    const result = {
      checkedAt: now,
      available: previous?.available ?? null,
      commits: previous?.commits,
      warning: `Crow update check could not complete: ${errorMessage(error)}`,
    };
    await atomic(cache, result).catch(() => {});
    return { ...result, cached: !!previous };
  }
}
