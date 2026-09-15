import { join } from "node:path";
import { mkdir, appendFile, opendir, lstat } from "node:fs/promises";
import { checkout, guidance, publicationPatch } from "./inspection.mjs";
import { atomic, json, sleep, hash, isRecord } from "./util.mjs";
import { validateConfig } from "./config.mjs";
import { runReview } from "./provider.mjs";
import { validateReport } from "./report.mjs";
import { cleanup } from "./retention.mjs";

import type {
  CrowConfig,
  ReviewJob,
  GitHubPullRequest,
  Guidance,
  ReviewReport,
  RuntimeReviewSettings,
  ReviewState,
} from "./types.mjs";

export interface ClaimedWork {
  job: ReviewJob;
  pr: GitHubPullRequest;
  token: string;
}
interface JobResponse {
  cancel?: boolean;
  skip?: boolean;
  ok?: boolean;
}
export interface WorkerResponses {
  next: ClaimedWork | null;
  maintenance: {
    jobs: Pick<ReviewJob, "id" | "state" | "updatedAt" | "session">[];
    retentionDays: number;
  };
  ping: { ok: boolean; id: string };
  heartbeat: JobResponse;
  comparison: JobResponse;
  session: JobResponse;
  progress: JobResponse;
  report: JobResponse;
  failed: JobResponse;
}
export type WorkerAction = keyof WorkerResponses;
export type WorkerRequest = <A extends WorkerAction>(
  config: CrowConfig,
  action: A,
  body?: object,
  signal?: AbortSignal,
) => Promise<WorkerResponses[A]>;
interface WorkerOptions {
  request?: WorkerRequest;
  review?: typeof runReview;
  prepare?: typeof checkout;
  readGuidance?: typeof guidance;
  readDiff?: typeof publicationPatch;
  logger?: Pick<Console, "error">;
  heartbeatMs?: number;
  pollMs?: number;
  reconnectMs?: number;
  maintenanceMs?: number;
  statusMs?: number;
}
interface ActiveReview {
  abort: AbortController;
  task: Promise<void>;
  repo: string;
  number: number;
}
interface SavedReport {
  head: string;
  base: string;
  target: string;
  report: ReviewReport;
}

export async function workerRequest<A extends WorkerAction>(
  config: CrowConfig,
  action: A,
  body: object = {},
  signal?: AbortSignal,
): Promise<WorkerResponses[A]> {
  const response = await fetch(
    `${config.serviceUrl.replace(/\/$/, "")}/worker/${action}`,
    {
      method: "POST",
      signal: signal
        ? AbortSignal.any([signal, AbortSignal.timeout(35000)])
        : AbortSignal.timeout(35000),
      headers: {
        Authorization: `Bearer ${config.worker.token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    },
  );
  const result: unknown = await response.json();
  if (!response.ok)
    throw new Error(
      result &&
        typeof result === "object" &&
        "error" in result &&
        typeof result.error === "string"
        ? result.error
        : `Crow service returned ${response.status}`,
    );
  return responseParsers(config)[action](result);
}

const jobId = /^[a-zA-Z0-9_-]{1,100}$/;
const sessionId =
  /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/;

function invalidResponse(): never {
  throw new Error("Crow service returned an invalid worker response");
}
function responseObject(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) invalidResponse();
  return value;
}
function responseString(value: unknown): string {
  if (typeof value !== "string") invalidResponse();
  return value;
}
function responseNumber(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) invalidResponse();
  return value;
}
function responseState(value: unknown): ReviewState {
  switch (value) {
    case "queued":
    case "held":
    case "reviewing":
    case "retrying":
    case "paused":
    case "publishing":
    case "completed":
    case "superseded":
    case "cancelled":
      return value;
    default:
      return invalidResponse();
  }
}
function responseSession(value: unknown): string | null | undefined {
  if (value === null || value === undefined) return value;
  if (typeof value !== "string" || !sessionId.test(value)) invalidResponse();
  return value;
}
function parseJob(value: unknown, config: CrowConfig): ReviewJob {
  const job = responseObject(value);
  for (const key of ["id", "key", "repo", "head", "target", "worker"])
    responseString(job[key]);
  if (!jobId.test(responseString(job.id))) invalidResponse();
  for (const key of [
    "number",
    "priority",
    "resumeEpoch",
    "createdAt",
    "updatedAt",
    "retries",
    "nextAt",
  ])
    responseNumber(job[key]);
  for (const key of ["manual", "restart"])
    if (typeof job[key] !== "boolean") invalidResponse();
  responseState(job.state);
  responseSession(job.session);
  for (const key of [
    "patch",
    "lease",
    "author",
    "reviewUrl",
    "guidanceFingerprint",
    "guidanceTargetSha",
    "parentId",
    "taskId",
  ])
    if (job[key] !== undefined) responseString(job[key]);
  if (job.trigger !== undefined) responseString(job.trigger);
  for (const key of ["startedAt", "publishAt"])
    if (job[key] !== undefined) responseNumber(job[key]);
  if (job.reason !== undefined && job.reason !== null)
    responseString(job.reason);
  if (job.autoRecover !== undefined && typeof job.autoRecover !== "boolean")
    invalidResponse();
  if (job.comparison !== undefined) {
    const comparison = responseObject(job.comparison);
    for (const key of ["head", "base", "target", "targetSha"])
      responseString(comparison[key]);
  }
  if (job.prContext !== undefined) {
    const context = responseObject(job.prContext);
    responseString(context.title);
    responseString(context.body);
  }
  if (job.settingsChanges !== undefined) {
    if (!Array.isArray(job.settingsChanges)) invalidResponse();
    for (const item of job.settingsChanges) {
      const change = responseObject(item);
      if (change.model !== null) responseString(change.model);
      if (change.effort !== null) responseString(change.effort);
      responseNumber(change.at);
    }
  }
  if (job.settings !== undefined) {
    const settings = responseObject(job.settings);
    for (const key of ["model", "effort", "subagents", "retry", "timeoutMs"])
      if (!(key in settings)) invalidResponse();
    validateConfig({ ...config, worker: { ...config.worker, ...settings } });
  }
  if (job.report !== undefined && job.report !== null)
    job.report = validateReport(job.report);
  // All required and optional ReviewJob fields have been checked above.
  return value as ReviewJob;
}
function parsePullRequest(value: unknown): GitHubPullRequest {
  const pr = responseObject(value),
    user = responseObject(pr.user),
    head = responseObject(pr.head),
    base = responseObject(pr.base);
  if (typeof pr.draft !== "boolean") invalidResponse();
  if (pr.title !== undefined) responseString(pr.title);
  if (pr.body !== undefined && pr.body !== null) responseString(pr.body);
  if (pr.updated_at !== undefined) responseString(pr.updated_at);
  return {
    number: responseNumber(pr.number),
    state: responseString(pr.state),
    draft: pr.draft,
    user: { login: responseString(user.login) },
    head: { sha: responseString(head.sha) },
    base: { sha: responseString(base.sha), ref: responseString(base.ref) },
    ...(pr.title !== undefined ? { title: responseString(pr.title) } : {}),
    ...(pr.body !== undefined
      ? { body: pr.body === null ? null : responseString(pr.body) }
      : {}),
    ...(pr.updated_at !== undefined
      ? { updated_at: responseString(pr.updated_at) }
      : {}),
  };
}
function parseFlags(value: unknown): JobResponse {
  const flags = responseObject(value);
  for (const key of ["cancel", "skip", "ok"])
    if (flags[key] !== undefined && typeof flags[key] !== "boolean")
      invalidResponse();
  return {
    ...(typeof flags.cancel === "boolean" ? { cancel: flags.cancel } : {}),
    ...(typeof flags.skip === "boolean" ? { skip: flags.skip } : {}),
    ...(typeof flags.ok === "boolean" ? { ok: flags.ok } : {}),
  };
}
function responseParsers(config: CrowConfig): {
  [A in WorkerAction]: (value: unknown) => WorkerResponses[A];
} {
  return {
    next(value) {
      if (value === null) return null;
      const work = responseObject(value);
      return {
        job: parseJob(work.job, config),
        pr: parsePullRequest(work.pr),
        token: responseString(work.token),
      };
    },
    maintenance(value) {
      const result = responseObject(value);
      if (!Array.isArray(result.jobs)) invalidResponse();
      return {
        jobs: result.jobs.map((value: unknown) => {
          const job = responseObject(value),
            id = responseString(job.id);
          if (!jobId.test(id)) invalidResponse();
          return {
            id,
            state: responseState(job.state),
            updatedAt: responseNumber(job.updatedAt),
            session: responseSession(job.session),
          };
        }),
        retentionDays: responseNumber(result.retentionDays),
      };
    },
    ping(value) {
      const result = responseObject(value);
      if (typeof result.ok !== "boolean") invalidResponse();
      return { ok: result.ok, id: responseString(result.id) };
    },
    heartbeat: parseFlags,
    comparison: parseFlags,
    session: parseFlags,
    progress: parseFlags,
    report: parseFlags,
    failed: parseFlags,
  };
}

async function savedSessions(root: string) {
  const sessions = new Map<string, string>(),
    path = join(root, "reviews");
  try {
    if (!(await lstat(path)).isDirectory()) return sessions;
    const entries = await opendir(path);
    let count = 0;
    for await (const entry of entries) {
      if (++count > 10000) break;
      if (!entry.isDirectory() || !jobId.test(entry.name)) continue;
      const file = join(path, entry.name, "session.json");
      try {
        const stat = await lstat(file);
        if (!stat.isFile() || stat.size > 1048576) continue;
        const saved = await json<{ id?: string } | null>(file, null);
        if (saved?.id && sessionId.test(saved.id))
          sessions.set(entry.name, saved.id);
      } catch {
        /* Ignore incomplete or damaged local state; the provider reports restart required. */
      }
    }
  } catch (error) {
    if (!(error instanceof Error && "code" in error && error.code === "ENOENT"))
      throw error;
  }
  return sessions;
}

export async function startWorker(
  config: CrowConfig,
  root: string,
  {
    request = workerRequest,
    review = runReview,
    prepare = checkout,
    readGuidance = guidance,
    readDiff = publicationPatch,
    logger = console,
    heartbeatMs = 10000,
    pollMs = 1000,
    reconnectMs = 5000,
    maintenanceMs = 3600000,
    statusMs = 30000,
  }: WorkerOptions = {},
) {
  const controller = new AbortController(),
    active = new Map<string, ActiveReview>();
  let draining = false,
    closed = false,
    claiming: Promise<boolean | undefined> | null = null,
    connection = "connecting";
  const sessions = await savedSessions(root);
  let writingStatus = Promise.resolve();
  function status() {
    const value = {
      version: 1,
      pid: process.pid,
      updatedAt: Date.now(),
      state: closed ? "stopped" : draining ? "draining" : "running",
      connection: closed ? "stopped" : connection,
      active: [...active.entries()].map(([id, item]) => ({
        id,
        repo: item.repo,
        number: item.number,
        state: item.abort.signal.aborted ? "stopping" : "reviewing",
      })),
    };
    writingStatus = writingStatus
      .then(() => atomic(join(root, "worker-status.json"), value))
      .catch(() => {
        logger.error(
          "Worker status could not be saved; inspect local file permissions.",
        );
      });
    return writingStatus;
  }
  async function rpc<A extends WorkerAction>(
    action: A,
    body: object = {},
    signal?: AbortSignal,
  ): Promise<WorkerResponses[A]> {
    try {
      const result = await request(config, action, body, signal);
      if (!closed && connection !== "connected") {
        connection = "connected";
        void status();
      }
      return result;
    } catch (error) {
      if (!closed && connection !== "unavailable") {
        connection = "unavailable";
        void status();
      }
      throw error;
    }
  }
  await status();
  await mkdir(join(root, "logs"), { recursive: true, mode: 0o700 });
  const defaults = Object.fromEntries(
    (
      [
        "model",
        "effort",
        "subagents",
        "retry",
        "timeoutMs",
        "concurrency",
      ] as const
    ).map((key) => [key, config.worker[key]]),
  );
  async function log(
    job: Pick<ReviewJob, "id">,
    error: unknown,
    token?: string,
  ) {
    let message = String(
      error instanceof Error ? error.stack || error.message : error,
    );
    const secrets = [
      config.worker.token,
      config.adminToken,
      token,
      token && Buffer.from(`x-access-token:${token}`).toString("base64"),
    ].filter(
      (value): value is string => typeof value === "string" && value.length > 0,
    );
    for (const secret of secrets)
      message = message.replaceAll(secret, "[redacted]");
    message = message
      .replace(/(?:gh[psuor]_[\w]+|github_pat_[\w]+|sk-[\w-]+)/g, "[redacted]")
      .replace(
        /(Authorization\s*[:=]\s*(?:Bearer|Basic)\s+)\S+/gi,
        "$1[redacted]",
      );
    await appendFile(
      join(root, "logs", `${job.id}.log`),
      `${new Date().toISOString()} ${message}\n`,
      { mode: 0o600 },
    );
  }
  async function execute(work: ClaimedWork, abort: AbortController) {
    const { token: _token, id: _id, ...workerSettings } = config.worker;
    const job: ReviewJob & { settings: RuntimeReviewSettings } = {
      ...work.job,
      settings: { ...workerSettings, ...work.job.settings },
      prContext: {
        title: String(work.pr?.title || "").slice(0, 1000),
        body: String(work.pr?.body || "").slice(0, 16000),
      },
    };
    const send = <
      A extends Exclude<WorkerAction, "next" | "maintenance" | "ping">,
    >(
      action: A,
      body: object = {},
    ) => rpc(action, { id: job.id, lease: job.lease, ...body });
    let heartbeats = Promise.resolve();
    const heartbeat = setInterval(() => {
      heartbeats = heartbeats.then(async () => {
        if (abort.signal.aborted) return;
        try {
          const result = await send("heartbeat");
          if (result.cancel)
            abort.abort(new Error("Review superseded or paused"));
        } catch {
          abort.abort(
            new Error("Connection service unavailable; preserving review"),
          );
        }
      });
    }, heartbeatMs);
    try {
      abort.signal.throwIfAborted();
      const source = await prepare(root, job, work.pr, work.token, {
        signal: abort.signal,
      });
      abort.signal.throwIfAborted();
      const rulesFile = join(root, "reviews", job.id, "guidance.json");
      let rules: Guidance | null;
      if (job.guidanceFingerprint) {
        rules = await json<Guidance | null>(rulesFile, null);
        if (!rules) {
          const targetSha = job.guidanceTargetSha || job.comparison?.targetSha;
          if (!targetSha)
            throw Object.assign(
              new Error(
                "Original review guidance revision is unavailable. Restart required.",
              ),
              { kind: "restart" },
            );
          try {
            rules = {
              ...(await readGuidance({ ...source, targetSha })),
              targetSha,
            };
          } catch {
            throw Object.assign(
              new Error(
                "Original review guidance cannot be reconstructed. Restart required.",
              ),
              { kind: "restart" },
            );
          }
        }
        if (
          rules.fingerprint !== job.guidanceFingerprint ||
          hash(rules.files) !== job.guidanceFingerprint
        )
          throw Object.assign(
            new Error(
              "Saved review guidance does not match the original review. Restart required.",
            ),
            { kind: "restart" },
          );
      } else {
        rules = {
          ...(await readGuidance(source)),
          targetSha: source.targetSha,
        };
      }
      await atomic(rulesFile, rules);
      abort.signal.throwIfAborted();
      job.comparison = source;
      job.guidanceFingerprint = rules.fingerprint;
      job.guidanceTargetSha = rules.targetSha;
      const decision = await send("comparison", {
        comparison: source,
        guidanceFingerprint: rules.fingerprint,
        guidanceTargetSha: rules.targetSha,
      });
      if (decision.cancel || decision.skip) return;
      abort.signal.throwIfAborted();
      const local = join(root, "reports", `${job.id}.json`);
      const saved = await json<SavedReport | null>(local, null);
      let report = job.report;
      if (
        !report &&
        saved &&
        saved.head === source.head &&
        saved.base === source.base &&
        saved.target === source.target
      )
        report = saved.report;
      if (!report) {
        report = await review({
          root,
          job: { ...job, comparison: source },
          source,
          guidance: rules,
          signal: abort.signal,
          onSession: async (session) => {
            job.session = session;
            if (sessionId.test(session)) sessions.set(job.id, session);
            const result = await send("session", { session });
            if (result.cancel)
              abort.abort(new Error("Review no longer owns its job"));
          },
          onProgress: async (event) => {
            if (event?.type === "warning") {
              logger.error(
                "The provider model list could not be refreshed; Crow is using its cached catalog. Run crow models for details.",
              );
              await log(job, new Error(event.message), work.token);
              return;
            }
            const result = await send("progress");
            if (result.cancel)
              abort.abort(new Error("Review no longer owns its job"));
          },
        });
      }
      report = validateReport(report);
      // Save completed output even if cancellation races completion. It never publishes after cancellation.
      await atomic(local, {
        head: source.head,
        base: source.base,
        target: source.target,
        report,
      });
      abort.signal.throwIfAborted();
      const patch = await readDiff(source, report.findings, {
        signal: abort.signal,
      });
      abort.signal.throwIfAborted();
      await send("report", { report, patch });
    } catch (error) {
      try {
        await log(job, error, work.token);
      } catch {
        /* Logging must not prevent saving failure state. */
      }
      logger.error(
        `Review ${job.repo}#${job.number} interrupted; see local logs.`,
      );
      try {
        await send("failed", {
          kind: abort.signal.aborted
            ? "interrupted"
            : error instanceof Error && "kind" in error
              ? error.kind
              : "transient",
          retryAfter:
            error instanceof Error && "retryAfter" in error
              ? error.retryAfter || 0
              : 0,
          ...(job.session && sessionId.test(job.session)
            ? { session: job.session }
            : {}),
        });
      } catch {
        /* Lost leases pause at the service; local sessions and reports remain saved. */
      }
    } finally {
      clearInterval(heartbeat);
      await heartbeats;
    }
  }
  async function claim() {
    const work = await rpc(
      "next",
      {
        defaults,
        active: [...active.keys()],
        sessions: Object.fromEntries(sessions),
      },
      controller.signal,
    );
    if (!work) return;
    if (!jobId.test(work.job?.id || ""))
      throw new Error("Invalid review job identifier");
    if (active.has(work.job.id))
      throw new Error("Service returned a job that is already active");
    const abort = new AbortController();
    if (closed) abort.abort(new Error("Worker stopped"));
    const task = execute(work, abort).finally(() => {
      active.delete(work.job.id);
      return status();
    });
    active.set(work.job.id, {
      abort,
      task,
      repo: work.job.repo,
      number: work.job.number,
    });
    abort.signal.addEventListener(
      "abort",
      () => {
        void status();
      },
      { once: true },
    );
    void status();
    return true;
  }
  async function loop() {
    while (!closed) {
      try {
        if (!draining && active.size < config.worker.concurrency) {
          claiming = claim();
          const claimed = await claiming;
          claiming = null;
          if (claimed && !draining && active.size < config.worker.concurrency)
            continue;
        }
        await sleep(pollMs, controller.signal);
      } catch {
        claiming = null;
        if (closed) break;
        logger.error("Worker connection unavailable; reconnecting.");
        try {
          await sleep(reconnectMs, controller.signal);
        } catch {
          break;
        }
      }
    }
  }
  let maintaining: Promise<void> | null = null;
  function maintain() {
    if (closed || maintaining) return maintaining;
    maintaining = (async () => {
      try {
        const result = await rpc("maintenance", {}, controller.signal);
        if (Array.isArray(result?.jobs)) {
          for (const job of result.jobs)
            if (["completed", "superseded", "cancelled"].includes(job.state))
              sessions.delete(job.id);
          const outcome = await cleanup(
            root,
            result.jobs.filter((job) => !active.has(job.id)),
            result.retentionDays,
          );
          if (outcome.warnings.length)
            logger.error(
              "Some retained review files could not be cleaned; inspect local file permissions.",
            );
        }
      } catch {
        if (!closed)
          logger.error(
            "Review retention cleanup deferred until the next maintenance check.",
          );
      }
    })().finally(() => {
      maintaining = null;
    });
    return maintaining;
  }
  const maintenance = setInterval(maintain, maintenanceMs);
  const statusTimer = setInterval(() => {
    void status();
  }, statusMs);
  void maintain();
  const running = loop();
  return {
    drain: async () => {
      draining = true;
      await status();
      // A claim already sent may have acquired a lease; include that work in the drain.
      await claiming?.catch(() => {});
      await Promise.all([...active.values()].map((item) => item.task));
    },
    close: async () => {
      closed = true;
      clearInterval(maintenance);
      clearInterval(statusTimer);
      controller.abort(new Error("Worker stopped"));
      for (const item of active.values())
        item.abort.abort(new Error("Worker stopped"));
      await running;
      await maintaining;
      await Promise.all([...active.values()].map((item) => item.task));
      await status();
    },
  };
}
