import type { CrowConfig, WorkerConfig, RepositorySettings } from "./types.mjs";
function object(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error(`Invalid ${label}`);
  return value as Record<string, unknown>;
}
function string(value: unknown, label: string): string {
  if (typeof value !== "string" || !value) throw new Error(`Invalid ${label}`);
  return value;
}
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { atomic, json, id, integer, httpsUrl } from "./util.mjs";
export function home() {
  return resolve(process.env.CROW_HOME || join(homedir(), ".local/share/crow"));
}
export function defaults(root = home()): CrowConfig {
  return {
    version: 1,
    role: "both",
    operator: null,
    publicUrl: null,
    port: 8787,
    bind: "127.0.0.1",
    adminToken: id() + id(),
    serviceUrl: "http://127.0.0.1:8787",
    worker: {
      id: id(),
      token: id() + id(),
      concurrency: 3,
      codex: "codex",
      codexHome: join(root, "codex"),
      model: null,
      effort: null,
      subagents: { mode: "inherit", max: 8 },
      retry: { mode: "fixed", count: 10, delayMs: 5000 },
      timeoutMs: 0,
    },
    catchUp: { enabled: true, threshold: 10 },
    auditIntervalMs: 3600000,
    retentionDays: 7,
    ingress: { type: "funnel" },
    app: null,
  };
}
export function validateConfig(value: unknown): CrowConfig {
  const c = object(value, "configuration");
  const worker = object(c.worker, "worker configuration");
  const subagents = object(worker.subagents, "subagent settings");
  const retry = object(worker.retry, "retry settings");
  const catchUp = object(c.catchUp, "catch-up settings");
  if (
    !c ||
    c.version !== 1 ||
    typeof c.role !== "string" ||
    !["both", "service", "worker"].includes(c.role)
  )
    throw new Error("Unsupported configuration version or role");
  if (!worker.subagents || !retry || !c.catchUp)
    throw new Error("Incomplete worker or recovery configuration");
  for (const token of [c.adminToken, worker.token])
    if (typeof token !== "string" || token.length < 32)
      throw new Error(
        "Crow administrative and pairing tokens need at least 32 characters",
      );
  if (
    typeof worker.id !== "string" ||
    !/^[A-Za-z0-9_-]{1,100}$/.test(worker.id)
  )
    throw new Error("Invalid worker identifier");
  const service = new URL(string(c.serviceUrl, "serviceUrl"));
  if (
    service.username ||
    service.password ||
    service.search ||
    service.hash ||
    service.pathname !== "/" ||
    (service.protocol !== "https:" &&
      !(
        service.protocol === "http:" &&
        ["127.0.0.1", "localhost", "[::1]"].includes(service.hostname)
      ))
  )
    throw new Error("Worker connections need HTTPS, or HTTP on localhost");
  for (const value of [worker.model, worker.effort])
    if (value !== null && (typeof value !== "string" || !value.trim()))
      throw new Error(
        "Model and reasoning selections must be nonempty strings",
      );
  if (typeof catchUp.enabled !== "boolean")
    throw new Error("catchUp.enabled must be a boolean");
  integer(c.port, 1, 65535, "port");
  integer(worker.concurrency, 1, 100, "worker.concurrency");
  integer(subagents.max, 0, 100, "subagents.max");
  if (
    typeof subagents.mode !== "string" ||
    !["inherit", "configured"].includes(subagents.mode)
  )
    throw new Error("Subagent mode must be inherit or configured");
  if (
    subagents.mode === "configured" &&
    (typeof subagents.model !== "string" ||
      !subagents.model.trim() ||
      typeof subagents.effort !== "string" ||
      !subagents.effort.trim())
  )
    throw new Error("Configured subagents need model and effort");
  if (
    typeof retry.mode !== "string" ||
    !["fixed", "progressive"].includes(retry.mode)
  )
    throw new Error("Unknown retry mode");
  integer(retry.count, 0, 100, "retry.count");
  integer(retry.delayMs, 1, 86400000, "retry.delayMs");
  integer(worker.timeoutMs, 0, 604800000, "timeoutMs");
  integer(catchUp.threshold, 1, 10000, "catchUp.threshold");
  integer(c.retentionDays, 1, 36500, "retentionDays");
  integer(c.auditIntervalMs, 60000, 604800000, "auditIntervalMs");
  if (c.publicUrl) httpsUrl(string(c.publicUrl, "publicUrl"));
  for (const field of ["bind", "serviceUrl"]) string(c[field], field);
  for (const field of ["codex", "codexHome"])
    string(worker[field], `worker.${field}`);
  if (c.operator !== null) string(c.operator, "operator");
  if (c.publicUrl !== null) string(c.publicUrl, "publicUrl");
  const ingress = object(c.ingress, "ingress");
  if (
    typeof ingress.type !== "string" ||
    !["funnel", "cloudflare", "existing"].includes(ingress.type)
  )
    throw new Error("Invalid ingress type");
  if (c.app !== null) {
    const app = object(c.app, "GitHub App");
    if (typeof app.id !== "number" && typeof app.id !== "string")
      throw new Error("Invalid GitHub App ID");
    for (const field of ["pem", "webhookSecret", "slug"])
      string(app[field], `app.${field}`);
    if (app.botId != null && typeof app.botId !== "number")
      throw new Error("Invalid GitHub bot ID");
  }
  if (worker.detached !== undefined && typeof worker.detached !== "boolean")
    throw new Error("Invalid worker.detached");
  for (const field of ["model", "effort"])
    if (subagents[field] !== undefined)
      string(subagents[field], `subagents.${field}`);
  if (ingress.port !== undefined)
    integer(ingress.port, 1, 65535, "ingress.port");
  for (const field of ["target", "file", "tunnel"])
    if (ingress[field] !== undefined)
      string(ingress[field], `ingress.${field}`);
  if (ingress.pending !== undefined && typeof ingress.pending !== "boolean")
    throw new Error("Invalid ingress.pending");
  if (c.appRegistration !== undefined) {
    const registration = object(c.appRegistration, "App registration");
    if (
      registration.ownerType !== "personal" &&
      registration.ownerType !== "organization"
    )
      throw new Error("Invalid App owner type");
    if (
      registration.visibility !== "public" &&
      registration.visibility !== "private"
    )
      throw new Error("Invalid App visibility");
    if (registration.organization !== undefined)
      string(registration.organization, "App organization");
  }
  return c as unknown as CrowConfig;
}
export async function load(root = home()) {
  const c = await json(join(root, "config.json"), null);
  if (!c) throw new Error("Crow is not configured. Run crow setup.");
  return validateConfig(c);
}
export async function save(c: CrowConfig, root = home()) {
  validateConfig(c);
  await atomic(join(root, "config.json"), c);
}
export function parseRepositorySettings(value: unknown): RepositorySettings {
  const o = object(value, "repository review settings");
  if (
    Object.keys(o).some(
      (key) =>
        !["model", "effort", "timeoutMs", "subagents", "retry"].includes(key),
    )
  )
    throw new Error("Unknown repository review setting");
  for (const key of ["model", "effort"])
    if (
      o[key] !== undefined &&
      o[key] !== null &&
      (typeof o[key] !== "string" || !o[key].trim())
    )
      throw new Error(
        "Model and reasoning selections must be nonempty strings",
      );
  if (o.timeoutMs !== undefined)
    integer(o.timeoutMs, 0, 604800000, "timeoutMs");
  if (o.subagents !== undefined) {
    const sub = object(o.subagents, "subagent overrides");
    if (
      sub.mode !== undefined &&
      sub.mode !== "inherit" &&
      sub.mode !== "configured"
    )
      throw new Error("Subagent mode must be inherit or configured");
    if (sub.max !== undefined) integer(sub.max, 0, 100, "subagents.max");
    for (const key of ["model", "effort"])
      if (sub[key] !== undefined) string(sub[key], `subagents.${key}`);
  }
  if (o.retry !== undefined) {
    const retry = object(o.retry, "retry overrides");
    if (
      retry.mode !== undefined &&
      retry.mode !== "fixed" &&
      retry.mode !== "progressive"
    )
      throw new Error("Unknown retry mode");
    if (retry.count !== undefined) integer(retry.count, 0, 100, "retry.count");
    if (retry.delayMs !== undefined)
      integer(retry.delayMs, 1, 86400000, "retry.delayMs");
  }
  return o as RepositorySettings;
}
export function settings(
  config: CrowConfig,
  repo: { settings?: unknown } | null | undefined,
): WorkerConfig {
  const w = structuredClone(config.worker),
    overrides = parseRepositorySettings(repo?.settings ?? {});
  const merged: Record<string, unknown> = { ...w };
  for (const key of ["model", "effort", "timeoutMs"] as const)
    if (overrides[key] !== undefined) merged[key] = overrides[key];
  if (overrides.subagents)
    merged.subagents = { ...w.subagents, ...overrides.subagents };
  if (overrides.retry) merged.retry = { ...w.retry, ...overrides.retry };
  return validateConfig({ ...config, worker: merged }).worker;
}
