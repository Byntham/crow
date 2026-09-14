import { createServer, type IncomingMessage } from "node:http";
import { join } from "node:path";
import { rm } from "node:fs/promises";
import { Store, eligible, retryDelay, comparisonKey } from "./store.mjs";
import { GitHub, verify } from "./github.mjs";
import {
  settings,
  validateConfig,
  parseRepositorySettings,
} from "./config.mjs";
import { id, equal, repoName, json } from "./util.mjs";
import {
  validateReport,
  reportBody,
  inlineComments,
  metadata,
} from "./report.mjs";
import type {
  CrowConfig,
  CrowEvent,
  ReviewJob,
  ReviewState,
  RepositoryRecord,
  WorkerRecord,
  Comparison,
  ReviewSettings,
  PublishableReviewJob,
  RepositorySettings,
} from "./types.mjs";

const object = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Expected an object");
  return value as Record<string, unknown>;
};
const optionalObject = (value: unknown): Record<string, unknown> =>
  value == null ? {} : object(value);
const string = (value: unknown, label = "value"): string => {
  if (typeof value !== "string") throw new Error(`Invalid ${label}`);
  return value;
};
const optionalString = (value: unknown): string | undefined =>
  value === undefined ? undefined : string(value);
const optionalNumber = (value: unknown): number | undefined => {
  if (value === undefined) return undefined;
  if (typeof value !== "number" || !Number.isFinite(value))
    throw new Error("Invalid number");
  return value;
};
const errorMessage = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);
const errorNumber = (error: unknown, key: "status" | "retryAfter"): number => {
  if (!error || typeof error !== "object" || !(key in error)) return 0;
  const value: unknown = Reflect.get(error, key);
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
};
function reviewSettings(value: ReviewSettings): ReviewSettings {
  return {
    model: value.model,
    effort: value.effort,
    subagents: value.subagents,
    retry: value.retry,
    timeoutMs: value.timeoutMs,
  };
}
interface AdminArguments {
  id?: string;
  token?: string;
  repo?: string;
  githubToken?: string;
  worker?: string;
  policy?: "selected" | "everyone";
  authors?: string[];
  requesters?: string[];
  settings?: RepositorySettings;
  includeBacklog?: boolean;
  number?: number;
  model?: string;
  effort?: string;
}
function adminArguments(value: unknown): AdminArguments {
  const a = object(value);
  if (
    a.policy !== undefined &&
    a.policy !== "selected" &&
    a.policy !== "everyone"
  )
    throw new Error("Invalid author policy");
  if (a.authors !== undefined && !logins(a.authors))
    throw new Error("Invalid authors");
  if (a.requesters !== undefined && !logins(a.requesters))
    throw new Error("Invalid requesters");
  if (a.includeBacklog !== undefined && typeof a.includeBacklog !== "boolean")
    throw new Error("Invalid backlog option");
  return {
    id: optionalString(a.id),
    token: optionalString(a.token),
    repo: optionalString(a.repo),
    githubToken: optionalString(a.githubToken),
    worker: optionalString(a.worker),
    policy: a.policy,
    authors: a.authors,
    requesters: a.requesters,
    settings:
      a.settings === undefined
        ? undefined
        : parseRepositorySettings(a.settings),
    includeBacklog: a.includeBacklog,
    number:
      a.number === undefined ? undefined : optionalNumber(Number(a.number)),
    model: optionalString(a.model),
    effort: optionalString(a.effort),
  };
}
interface CatchUpResult {
  queued: number;
  held: number;
}
const activeStates: ReviewState[] = [
  "queued",
  "held",
  "reviewing",
  "retrying",
  "paused",
  "publishing",
];
const logins = (value: unknown): value is string[] =>
  Array.isArray(value) &&
  value.every(
    (name) =>
      typeof name === "string" &&
      /^[A-Za-z0-9][A-Za-z0-9-]{0,38}(?:\[bot\])?$/.test(name),
  );
function publicJob(j: ReviewJob) {
  const { report, lease, patch, ...view } = j;
  return view;
}
function extractEvent(type: unknown, payload: unknown): CrowEvent {
  if (type === "ping") return { type };
  const p = object(payload),
    repository = optionalObject(p.repository);
  const repo = optionalString(repository.full_name);
  if (!repo) return { type: "ignored" };
  if (type === "pull_request") {
    const pr = optionalObject(p.pull_request);
    return {
      type,
      repo,
      number: optionalNumber(p.number),
      action: optionalString(p.action),
      ...(pr.updated_at
        ? { occurredAt: Date.parse(string(pr.updated_at)) }
        : {}),
    };
  }
  const issue = optionalObject(p.issue);
  if (
    type === "issue_comment" &&
    issue.pull_request &&
    p.action === "created"
  ) {
    const comment = optionalObject(p.comment),
      user = optionalObject(comment.user);
    return {
      type,
      repo,
      number: optionalNumber(issue.number),
      request: optionalString(comment.body)?.trim() === "@crow review",
      actor: optionalString(user.login),
      ...(comment.created_at
        ? { occurredAt: Date.parse(string(comment.created_at)) }
        : {}),
    };
  }
  if (type === "push") return { type, repo, ref: optionalString(p.ref) };
  return { type: "ignored" };
}
export async function requestBody(req: IncomingMessage, max = 2 * 1024 * 1024) {
  const chunks: Buffer[] = [];
  let n = 0;
  for await (const chunk of req) {
    const c = Buffer.isBuffer(chunk) ? chunk : Buffer.from(string(chunk));
    n += c.length;
    if (n > max)
      throw Object.assign(new Error("Request too large"), { status: 413 });
    chunks.push(c);
  }
  return Buffer.concat(chunks);
}
export async function startService(
  config: CrowConfig,
  root: string,
  {
    github = new GitHub(config.app),
    store = new Store(join(root, "service.sqlite")),
    logger = console,
  }: { github?: GitHub; store?: Store; logger?: Pick<Console, "error"> } = {},
) {
  let closed = false,
    busy = false,
    publishing = false,
    auditing = false;
  const scans = new Map<
    string,
    { includeBacklog: boolean; task: Promise<CatchUpResult> }
  >();
  if (config.role === "both")
    store.put("workers", config.worker.id, {
      ...store.get("workers", config.worker.id),
      id: config.worker.id,
      token: config.worker.token,
    });
  const restore = await json(join(root, "restore-pending.json"), null);
  for (const j of store.all("jobs"))
    if (
      j.state === "reviewing" ||
      (restore && ["queued", "retrying"].includes(j.state))
    )
      store.updateJob(j.id, {
        state: "paused",
        autoRecover: !restore,
        reason: restore
          ? "Backup restored. Resume after checking worker availability."
          : "Service restarted; waiting for the worker to reconnect.",
      });
  if (restore) await rm(join(root, "restore-pending.json"), { force: true });
  const repoFor = (name: unknown) => {
    const r = store.get("repos", repoName(string(name, "repository")));
    if (!r) throw new Error("Repository is not enrolled");
    return r;
  };
  async function refreshPr(repo: RepositoryRecord, n: number) {
    const token = await github.token(repo);
    return { pr: await github.pr(repo, n, token), token };
  }
  async function enqueue(
    repo: RepositoryRecord,
    n: number,
    {
      manual = false,
      restart = false,
      held = false,
      event = false,
      requester = "",
    } = {},
  ) {
    const { pr } = await refreshPr(repo, n);
    repo = repoFor(repo.name);
    if (
      requester &&
      !(repo.requesters || repo.authors).some(
        (login) => login.toLowerCase() === requester.toLowerCase(),
      )
    )
      return { skipped: "Requester is no longer authorized" };
    if (!eligible(repo, pr)) {
      for (const j of store
        .all("jobs")
        .filter(
          (j) =>
            j.repo === repo.name &&
            j.number === n &&
            activeStates.includes(j.state),
        ))
        store.updateJob(j.id, {
          state: "cancelled",
          reason: "PR is closed, a draft, or its author is not authorized",
        });
      return { skipped: "PR is not eligible" };
    }
    if (event || manual) {
      repo.excluded = (repo.excluded || []).filter((x) => x !== n);
      store.enroll(repo);
    } else if (repo.excluded?.includes(n))
      return { skipped: "Initial backlog excluded" };
    return store.queue(repo, pr, { manual, restart, held });
  }
  async function catchUp(
    name: string,
    includeBacklog = false,
  ): Promise<CatchUpResult> {
    const existing = scans.get(name);
    if (existing) {
      if (!includeBacklog || existing.includeBacklog) return existing.task;
      // An explicit inclusive scan must not inherit a normal scan's exclusions.
      await existing.task.catch(() => {});
      return catchUp(name, true);
    }
    const task = (async () => {
      const initial = repoFor(name),
        token = await github.token(initial),
        prs = await github.prs(initial, token),
        repo = repoFor(name);
      if (includeBacklog) {
        repo.excluded = [];
        store.enroll(repo);
      }
      const byNumber = new Map(prs.map((pr) => [pr.number, pr]));
      for (const job of store
        .all("jobs")
        .filter(
          (j) => j.repo === repo.name && activeStates.includes(j.state),
        )) {
        const current = byNumber.get(job.number);
        if (!current || !eligible(repo, current))
          store.updateJob(job.id, {
            state: "cancelled",
            autoRecover: false,
            reason: "PR is no longer eligible",
          });
      }
      const candidates = prs.filter(
        (pr) => eligible(repo, pr) && !repo.excluded?.includes(pr.number),
      );
      // Workers check the merge base before inference. Heads with a recorded comparison can still change on retargeting.
      const fresh = candidates.filter(
        (pr) =>
          !store
            .all("jobs")
            .some(
              (j) =>
                j.repo === name &&
                j.number === pr.number &&
                j.head === pr.head.sha &&
                j.target === pr.base.ref &&
                (activeStates.includes(j.state) ||
                  (j.state === "completed" &&
                    j.comparison?.targetSha === pr.base.sha)),
            ),
      );
      const held = fresh.length > config.catchUp.threshold;
      for (const pr of fresh) store.queue(repo, pr, { held });
      return { queued: held ? 0 : fresh.length, held: held ? fresh.length : 0 };
    })();
    scans.set(name, { includeBacklog, task });
    try {
      return await task;
    } finally {
      if (scans.get(name)?.task === task) scans.delete(name);
    }
  }
  async function handleEvent(e: CrowEvent) {
    if (!e.repo) return;
    const repo = store.get("repos", repoName(e.repo));
    if (!repo) return;
    if (e.occurredAt && e.occurredAt < repo.enrolledAt) return;
    if (
      e.type === "pull_request" &&
      [
        "opened",
        "reopened",
        "synchronize",
        "ready_for_review",
        "edited",
        "closed",
        "converted_to_draft",
      ].includes(e.action || "") &&
      e.number !== undefined
    )
      await enqueue(repo, e.number, { event: true });
    if (
      e.type === "issue_comment" &&
      e.request &&
      e.number !== undefined &&
      (repo.requesters || repo.authors).some(
        (x) => x.toLowerCase() === e.actor?.toLowerCase(),
      )
    )
      await enqueue(repo, e.number, { manual: true, requester: e.actor });
    if (e.type === "push" && e.ref?.startsWith("refs/heads/")) {
      // Event-driven target changes: inspect comparisons again without importing excluded backlog.
      const token = await github.token(repo);
      const prs = await github.prs(repo, token);
      const currentRepo = repoFor(repo.name);
      for (const pr of prs)
        if (
          pr.base.ref === e.ref.slice(11) &&
          eligible(currentRepo, pr) &&
          !currentRepo.excluded?.includes(pr.number)
        )
          store.queue(currentRepo, pr);
    }
  }
  async function status(j: ReviewJob) {
    if (!config.app?.botId) return;
    const repo = store.get("repos", j.repo);
    if (!repo) return;
    const current = store
      .all("jobs")
      .filter((x) => x.key === j.key)
      .at(-1);
    if (current?.id !== j.id) return;
    const text = `<!-- crow-status:v1 -->\nCrow: **${j.state}** · [${j.head.slice(0, 8)}](https://github.com/${j.repo}/commit/${j.head})\n\nUpdated ${new Date(j.updatedAt).toISOString()}.${j.reason ? `\n\n${j.reason}` : ""}${j.reviewUrl ? `\n\n[Read review](${j.reviewUrl})` : ""}${j.state === "paused" ? "\n\nUse `crow resume` to continue saved work, or `crow restart` to start again." : ""}`;
    const key = `${j.repo}#${j.number}`,
      prev = store.get("status", key);
    if (
      prev?.body === text ||
      (prev?.state === j.state && Date.now() - (prev.updatedAt || 0) < 60000)
    )
      return;
    const c = await github.status(
      repo,
      j.number,
      await github.token(repo),
      text,
      config.app?.botId,
      prev?.id,
    );
    store.put("status", key, {
      id: c.id,
      body: text,
      state: j.state,
      updatedAt: Date.now(),
    });
  }
  async function publish(j: PublishableReviewJob) {
    const { pr, token } = await refreshPr(repoFor(j.repo), j.number);
    const repo = repoFor(j.repo);
    // An operator may pause publication while GitHub is responding.
    if (store.get("jobs", j.id)?.state !== "publishing") return;
    if (
      !eligible(repo, pr) ||
      pr.head.sha !== j.head ||
      pr.base.ref !== j.target
    ) {
      store.updateJob(j.id, { state: "superseded" });
      if (eligible(repo, pr)) store.queue(repo, pr);
      return;
    }
    // Target SHA changing needs a worker comparison check before publishing. Do not label an unchecked comparison current.
    if (pr.base.sha !== j.comparison.targetSha) {
      store.updateJob(j.id, {
        state: "queued",
        nextAt: 0,
        reason:
          "Target branch changed; verifying the comparison before publication.",
      });
      return;
    }
    const reviews = await github.reviews(repo, j.number, token);
    let review: { id: number; html_url: string } | undefined = reviews.find(
      (r) =>
        !!config.app?.botId &&
        r.user?.id === config.app.botId &&
        metadata(r.body)?.job === j.id,
    );
    const history = store.get("findings", j.key) || [];
    if (!review && store.get("jobs", j.id)?.state !== "publishing") return;
    if (!review)
      review = await github.publish(
        repo,
        j.number,
        token,
        reportBody(
          j,
          history,
          reviews
            .filter(
              (r) =>
                !!config.app?.botId &&
                r.user?.id === config.app.botId &&
                metadata(r.body) &&
                metadata(r.body)?.job !== j.id,
            )
            .map((r) => ({ url: r.html_url, head: metadata(r.body)?.head })),
        ),
        j.head,
        inlineComments(j.report, j.patch || "", history),
      );
    const reviewUrl = review.html_url;
    const records = new Map(history.map((x) => [x.id, x]));
    for (const f of j.report.findings)
      records.set(f.id, { id: f.id, title: f.title, url: reviewUrl });
    store.tx(() => {
      store.put("findings", j.key, [...records.values()]);
      store.put("completed", `${j.key}:${comparisonKey(j.comparison)}`, {
        job: j.id,
        reviewUrl,
        comparison: j.comparison,
      });
    });
    const current = store.get("jobs", j.id);
    store.updateJob(j.id, {
      state: current?.state === "superseded" ? "superseded" : "completed",
      reviewUrl,
      reason:
        current?.state === "superseded"
          ? "A newer revision arrived during publication. This report covers the pinned earlier commit."
          : null,
    });
  }
  async function tick() {
    if (busy || closed) return;
    busy = true;
    try {
      const deferredRepos = new Set<string>();
      for (const e of store.events()) {
        const repository = e.repo?.toLowerCase() || e.id;
        if (deferredRepos.has(repository)) continue;
        if ((e.nextAt || 0) > Date.now()) {
          deferredRepos.add(repository);
          continue;
        }
        try {
          await handleEvent(e);
          store.eventDone(e.id);
        } catch (err) {
          const delay = Math.max(
            Math.min(5000 * 2 ** Math.min(e.retries || 0, 6), 300000),
            Math.min(errorNumber(err, "retryAfter"), 86400000),
          );
          store.deferEvent(e, Date.now() + delay);
          deferredRepos.add(repository);
          logger.error("Event deferred:", errorMessage(err));
        }
      }
      for (const j of store.all("jobs")) {
        if (j.state === "reviewing" && Date.now() - j.updatedAt > 90000)
          store.updateJob(j.id, {
            state: "paused",
            autoRecover: true,
            reason:
              "Worker disconnected. Saved work will resume when it reconnects.",
          });
      }
      const latest = new Map<string, ReviewJob>();
      for (const j of store.all("jobs")) latest.set(j.key, j);
      for (const j of latest.values())
        try {
          await status(j);
        } catch (e) {
          logger.error("GitHub status deferred:", errorMessage(e));
        }
    } finally {
      busy = false;
    }
  }
  async function publishTick() {
    if (publishing || closed) return;
    publishing = true;
    try {
      for (const j of store
        .all("jobs")
        .filter(
          (j) => j.state === "publishing" && (j.publishAt || 0) < Date.now(),
        ))
        try {
          if (!j.comparison || !j.report || !j.settings)
            throw new Error("Incomplete publication record");
          await publish({
            ...j,
            comparison: j.comparison,
            report: j.report,
            settings: j.settings,
          });
        } catch (e) {
          store.updateJob(j.id, {
            publishAt:
              Date.now() + Math.max(30000, errorNumber(e, "retryAfter")),
            reason:
              "GitHub publication failed; the completed report is saved and will be retried.",
          });
          logger.error("Publication deferred:", errorMessage(e));
        }
    } finally {
      publishing = false;
    }
  }
  async function audit() {
    if (auditing || closed || !config.app) return;
    auditing = true;
    try {
      store.prune(config.retentionDays);
      await github.audit();
    } catch (e) {
      logger.error("Delivery audit deferred:", errorMessage(e));
    } finally {
      auditing = false;
    }
  }
  async function admin(action: string, value: unknown) {
    const a = adminArguments(value);
    if (action === "pair") {
      const w = { id: a.id || id(), token: a.token || id() + id() };
      if (
        !/^[A-Za-z0-9_-]{1,100}$/.test(w.id) ||
        typeof w.token !== "string" ||
        w.token.length < 32
      )
        throw new Error("Invalid worker ID or pairing token");
      if (store.get("workers", w.id))
        throw new Error("Worker ID is already paired");
      store.put("workers", w.id, w);
      return w;
    }
    if (action === "enroll") {
      const name = repoName(string(a.repo, "repository"));
      if (store.get("repos", name))
        throw new Error(
          "Repository is already enrolled. Use crow config-repo to change it.",
        );
      if (!config.operator) throw new Error("Complete operator setup first");
      const user = object(
        await github.request("/user", { token: a.githubToken }),
      );
      if (string(user.login).toLowerCase() !== config.operator.toLowerCase())
        throw new Error("GitHub login does not match the Crow operator");
      const info = object(
        await github.request(`/repos/${name}`, {
          token: a.githubToken,
        }),
      );
      const permissions = optionalObject(info.permissions);
      if (!permissions.admin && !permissions.maintain)
        throw new Error(
          "Repository enrollment requires admin or maintain authority",
        );
      const installation = await github.installation(name),
        repo: RepositoryRecord = {
          name,
          installation: installation.id,
          worker: a.worker || config.worker.id,
          policy: a.policy || "selected",
          authors: a.authors || [config.operator],
          requesters: [config.operator],
          settings: a.settings || {},
          enrolledAt: Date.now(),
          excluded: [],
        };
      if (!store.get("workers", repo.worker))
        throw new Error("Pair the worker before enrolling repositories");
      if (
        !["selected", "everyone"].includes(repo.policy) ||
        !logins(repo.authors) ||
        !repo.authors.length
      )
        throw new Error("Invalid author policy");
      settings(config, repo);
      const prs = await github.prs(repo, await github.token(repo));
      repo.excluded = prs.map((p) => p.number);
      store.enroll(repo);
      if (a.includeBacklog) await catchUp(name, true);
      return repo;
    }
    if (action === "config-repo") {
      const repo = repoFor(a.repo);
      if (a.policy !== undefined) repo.policy = a.policy;
      if (a.authors !== undefined) repo.authors = a.authors;
      if (a.requesters !== undefined) repo.requesters = a.requesters;
      if (a.worker !== undefined) repo.worker = a.worker;
      if (a.settings !== undefined) repo.settings = a.settings;
      if (
        !["selected", "everyone"].includes(repo.policy) ||
        !logins(repo.authors) ||
        !logins(repo.requesters) ||
        !store.get("workers", repo.worker)
      )
        throw new Error("Invalid repository policy or worker");
      settings(config, repo);
      store.enroll(repo);
      for (const job of store.all("jobs")) {
        if (
          job.repo === repo.name &&
          job.author &&
          activeStates.includes(job.state) &&
          repo.policy !== "everyone" &&
          !repo.authors.some(
            (name) => name.toLowerCase() === job.author?.toLowerCase(),
          )
        )
          store.updateJob(job.id, {
            state: "cancelled",
            autoRecover: false,
            reason: "PR author is no longer authorized",
          });
      }
      return repo;
    }
    if (["review", "resume", "restart"].includes(action)) {
      if (action === "resume" && (a.model || a.effort)) {
        const current = store
          .all("jobs")
          .filter(
            (j) =>
              j.repo === repoName(string(a.repo, "repository")) &&
              j.number === Number(a.number),
          )
          .at(-1);
        if (
          current &&
          ["reviewing", "publishing", "queued", "retrying"].includes(
            current?.state,
          )
        )
          throw new Error(
            "Pause the review before changing its model or reasoning level.",
          );
      }
      const job = await enqueue(repoFor(a.repo), Number(a.number), {
        manual: true,
        restart: action === "restart",
      });
      if (action === "resume" && (a.model || a.effort) && "id" in job) {
        const repo = repoFor(a.repo),
          worker = store.get("workers", repo.worker);
        const chosen = reviewSettings({
          ...(job.settings ||
            settings(
              { ...config, worker: { ...config.worker, ...worker?.defaults } },
              repo,
            )),
          ...(a.model ? { model: a.model } : {}),
          ...(a.effort ? { effort: a.effort } : {}),
        });
        settings({ ...config, worker: { ...config.worker, ...chosen } }, null);
        store.updateJob(job.id, {
          settings: chosen,
          settingsChanges: [
            ...(job.settingsChanges || []),
            { model: chosen.model, effort: chosen.effort, at: Date.now() },
          ],
        });
      }
      return job;
    }
    if (action === "pause") {
      const repo = repoFor(a.repo);
      for (const j of store
        .all("jobs")
        .filter(
          (j) =>
            j.repo === repo.name &&
            j.number === Number(a.number) &&
            activeStates.includes(j.state),
        ))
        store.updateJob(j.id, {
          state: "paused",
          autoRecover: false,
          reason: "Paused by the operator",
        });
      return { paused: true };
    }
    if (action === "catch-up") {
      const result: Record<string, CatchUpResult> = {};
      for (const repo of a.repo ? [repoFor(a.repo)] : store.all("repos"))
        result[repo.name] = await catchUp(repo.name, !!a.includeBacklog);
      return result;
    }
    if (action === "release") {
      let n = 0;
      for (const j of store.all("jobs"))
        if (
          j.state === "held" &&
          (!a.repo || j.repo === repoName(string(a.repo, "repository")))
        ) {
          store.updateJob(j.id, { state: "queued" });
          n++;
        }
      return { released: n };
    }
    if (action === "cleanup") {
      store.prune(config.retentionDays);
      return { cleaned: true };
    }
    if (action === "drain") {
      store.put("state", "drain", true);
      return { draining: true };
    }
    if (action === "undrain") {
      store.delete("state", "drain");
      return { draining: false };
    }
    throw new Error("Unknown administrative action");
  }
  async function workerAction(
    action: string,
    value: unknown,
    worker: WorkerRecord,
  ) {
    const a = object(value);
    worker.lastSeen = Date.now();
    if (action === "next" && a.defaults) {
      const input = object(a.defaults);
      const candidate = { ...config.worker };
      const defaults = validateConfig({
        ...config,
        worker: {
          ...candidate,
          ...Object.fromEntries(
            [
              "model",
              "effort",
              "subagents",
              "retry",
              "timeoutMs",
              "concurrency",
            ]
              .filter((key) => input[key] !== undefined)
              .map((key) => [key, input[key]]),
          ),
        },
      }).worker;
      worker.defaults = {
        ...reviewSettings(defaults),
        concurrency: defaults.concurrency,
      };
    }
    store.put("workers", worker.id, worker);
    if (action === "next") {
      if (a.sessions && typeof a.sessions === "object")
        for (const [jobId, session] of Object.entries(a.sessions)) {
          const job = store.get("jobs", jobId);
          if (
            job?.worker === worker.id &&
            !job.session &&
            job.autoRecover &&
            job.state === "paused" &&
            typeof session === "string" &&
            /^[a-f0-9-]{36}$/.test(session)
          )
            store.updateJob(job.id, { session });
        }
      const active: string[] = Array.isArray(a.active)
        ? a.active.map((value) => string(value, "active job"))
        : [];
      if (Array.isArray(a.active))
        for (const job of store
          .all("jobs")
          .filter(
            (j) =>
              j.worker === worker.id &&
              j.autoRecover &&
              j.state === "paused" &&
              !active.includes(j.id),
          )) {
          store.updateJob(job.id, {
            state: job.session || job.report ? "queued" : "paused",
            autoRecover: false,
            reason:
              job.session || job.report
                ? null
                : "Restart required: no saved provider session",
          });
        }
      if (
        store.get("state", "drain") ||
        Number(store.get("state", "cooldown")) > Date.now()
      )
        return null;
      if (
        store
          .all("jobs")
          .filter((j) => j.worker === worker.id && j.state === "reviewing")
          .length >= (worker.defaults?.concurrency || config.worker.concurrency)
      )
        return null;
      const keys = active
        .map((key) => store.get("jobs", key))
        .filter((j): j is ReviewJob => j?.worker === worker.id)
        .map((j) => j.key);
      const job = store.claim(worker.id, {
        excludeIds: active,
        excludeKeys: keys,
      });
      if (!job) return null;
      try {
        const { pr, token } = await refreshPr(repoFor(job.repo), job.number),
          current = store.get("jobs", job.id);
        if (current?.state !== "reviewing" || current.lease !== job.lease)
          return null;
        const repo = repoFor(job.repo);
        if (
          !eligible(repo, pr) ||
          pr.head.sha !== job.head ||
          pr.base.ref !== job.target ||
          repo.worker !== job.worker
        ) {
          store.updateJob(job.id, { state: "superseded" }, job.lease);
          if (eligible(repo, pr)) store.queue(repo, pr);
          return null;
        }
        const effective = reviewSettings(
          job.settings ||
            settings(
              { ...config, worker: { ...config.worker, ...worker.defaults } },
              repo,
            ),
        );
        store.updateJob(
          job.id,
          { settings: effective, author: pr.user.login },
          job.lease,
        );
        return { job: { ...job, settings: effective }, pr, token, repo };
      } catch (e) {
        const current = store.get("jobs", job.id);
        if (current?.state === "reviewing" && current.lease === job.lease)
          store.updateJob(
            job.id,
            {
              state: "queued",
              nextAt: Date.now() + 30000,
              reason: "GitHub connection failed; dispatch will retry.",
            },
            job.lease,
          );
        throw e;
      }
    }
    if (action === "ping") return { ok: true, id: worker.id };
    if (action === "maintenance")
      return {
        jobs: store
          .all("jobs")
          .filter((j) => j.worker === worker.id)
          .map((j) => ({
            id: j.id,
            state: j.state,
            updatedAt: j.updatedAt,
            session: j.session,
          })),
        retentionDays: config.retentionDays,
      };
    const j = store.get("jobs", string(a.id, "job ID"));
    const lease = string(a.lease, "lease");
    if (
      !j ||
      j.worker !== worker.id ||
      j.lease !== lease ||
      j.state !== "reviewing"
    )
      return { cancel: true };
    if (action === "heartbeat") {
      store.updateJob(j.id, {}, lease);
      return { cancel: false };
    }
    if (action === "session") {
      if (typeof a.session !== "string" || !/^[a-f0-9-]{36}$/.test(a.session))
        throw new Error("Invalid session identifier");
      store.updateJob(j.id, { session: a.session }, lease);
      return { ok: true };
    }
    if (action === "progress") {
      store.updateJob(j.id, { retries: 0 }, lease);
      return { ok: true };
    }
    if (action === "comparison") {
      const input = object(a.comparison);
      const c: Comparison = {
        head: string(input.head),
        target: string(input.target),
        base: string(input.base),
        targetSha: string(input.targetSha),
      };
      if (
        !c ||
        c.head !== j.head ||
        c.target !== j.target ||
        !/^[a-f0-9]{40,64}$/.test(c.base) ||
        !/^[a-f0-9]{40,64}$/.test(c.targetSha)
      )
        throw new Error("Invalid comparison");
      if (j.comparison && comparisonKey(j.comparison) !== comparisonKey(c)) {
        store.updateJob(j.id, { state: "superseded" });
        await enqueue(repoFor(j.repo), j.number, { restart: true });
        return { cancel: true };
      }
      let previous = store.get("completed", `${j.key}:${comparisonKey(c)}`);
      if (!previous && !j.manual && config.app?.botId) {
        const repo = repoFor(j.repo),
          token = await github.token(repo);
        const published = (await github.reviews(repo, j.number, token)).find(
          (r) => {
            const m = metadata(r.body);
            return (
              r.user?.id === config.app?.botId &&
              m?.head === c.head &&
              m?.base === c.base &&
              m?.target === c.target
            );
          },
        );
        if (published) previous = { reviewUrl: published.html_url };
      }
      if (previous && !j.manual) {
        store.updateJob(
          j.id,
          {
            state: "completed",
            comparison: c,
            reviewUrl: previous.reviewUrl,
            reason: null,
          },
          lease,
        );
        return { skip: true };
      }
      store.updateJob(
        j.id,
        {
          comparison: c,
          guidanceFingerprint: optionalString(a.guidanceFingerprint),
          guidanceTargetSha: optionalString(a.guidanceTargetSha) || c.targetSha,
        },
        lease,
      );
      return { ok: true };
    }
    if (action === "report") {
      if (!j.comparison)
        throw new Error(
          "Comparison must be verified before a report is accepted",
        );
      const report = validateReport(a.report);
      store.updateJob(
        j.id,
        {
          report,
          patch: String(a.patch || "").slice(0, 1000000),
          state: "publishing",
          reason: null,
        },
        lease,
      );
      return { ok: true };
    }
    if (action === "failed") {
      if (
        !j.session &&
        typeof a.session === "string" &&
        /^[a-f0-9-]{36}$/.test(a.session)
      ) {
        j.session = a.session;
        store.updateJob(j.id, { session: a.session }, lease);
      }
      if (!j.settings)
        throw new Error("Review settings are missing from the active job");
      const kind = optionalString(a.kind) || "transient";
      if (kind === "superseded") {
        const { pr } = await refreshPr(repoFor(j.repo), j.number);
        const repo = repoFor(j.repo),
          current = store.get("jobs", j.id);
        if (current?.state !== "reviewing" || current.lease !== lease)
          return { cancel: true };
        store.updateJob(j.id, { state: "superseded" }, lease);
        if (eligible(repo, pr)) {
          const replacement = store.queue(repo, pr);
          // GitHub's PR snapshot may briefly lag the fetched Git reference.
          if (pr.head.sha === j.head && pr.base.ref === j.target)
            store.updateJob(replacement.id, { nextAt: Date.now() + 5000 });
        }
        return { cancel: true };
      }
      const count = j.retries + 1,
        retry = j.settings.retry;
      let state: ReviewState = "paused",
        reason = "Review interrupted. Resume saved work or explicitly restart.",
        nextAt = 0;
      if (kind === "transient" && j.session && count <= retry.count) {
        state = "retrying";
        nextAt =
          Date.now() +
          retryDelay(
            retry,
            count,
            Math.min(Number(a.retryAfter) || 0, 86400000),
          );
        reason = `Provider interrupted; retry ${count}/${retry.count} is scheduled.`;
        store.put(
          "state",
          "cooldown",
          Math.max(Number(store.get("state", "cooldown")) || 0, nextAt),
        );
      }
      if (kind === "auth")
        reason =
          "Codex subscription login needs attention on the worker. Then resume this review.";
      if (kind === "quota")
        reason =
          "Subscription usage is unavailable. Resume when quota is available.";
      if (kind === "config")
        reason =
          "The configured model or reasoning level is unavailable. Check Crow settings on the worker.";
      if (
        kind === "restart" ||
        (!j.session && !["auth", "quota", "config"].includes(kind))
      )
        reason = "Restart required: no usable saved provider session.";
      if (kind === "output")
        reason =
          "Codex did not return a valid completed report. Resume the saved session to correct it.";
      store.updateJob(j.id, { state, reason, retries: count, nextAt }, lease);
      return { ok: true };
    }
    throw new Error("Unknown worker action");
  }
  const server = createServer(async (req, res) => {
    const reply = (code: number, body: unknown) => {
      res.writeHead(code, {
        "Content-Type": "application/json",
        "Cache-Control": "no-store",
      });
      res.end(JSON.stringify(body));
    };
    try {
      const path = new URL(req.url || "/", "http://localhost").pathname;
      if (req.method === "GET" && path === "/health")
        return reply(200, {
          ok: true,
          service: "crow",
          configured: !!config.app,
        });
      if (path === "/webhooks/github" && req.method === "POST") {
        const raw = await requestBody(req);
        if (
          !verify(
            raw,
            typeof req.headers["x-hub-signature-256"] === "string"
              ? req.headers["x-hub-signature-256"]
              : undefined,
            config.app?.webhookSecret,
          )
        )
          return reply(401, { error: "Invalid webhook signature" });
        const delivery = req.headers["x-github-delivery"];
        if (typeof delivery !== "string" || delivery.length > 200)
          return reply(400, { error: "Missing delivery ID" });
        const accepted = store.acceptEvent(
          delivery,
          extractEvent(
            req.headers["x-github-event"],
            JSON.parse(raw.toString("utf8")),
          ),
        );
        return reply(202, { accepted });
      }
      const bearer = String(req.headers.authorization || "").replace(
        /^Bearer /,
        "",
      );
      if (path.startsWith("/admin/")) {
        if (!equal(bearer, config.adminToken))
          return reply(401, { error: "Unauthorized" });
        if (path === "/admin/status" && req.method === "GET")
          return reply(200, {
            repos: store.all("repos"),
            workers: store.all("workers").map(({ token, ...w }) => w),
            jobs: store.all("jobs").map(publicJob),
            draining: !!store.get("state", "drain"),
          });
        if (req.method !== "POST")
          return reply(405, { error: "POST required" });
        return reply(
          200,
          await admin(
            path.slice(7),
            JSON.parse((await requestBody(req)).toString("utf8")),
          ),
        );
      }
      if (path.startsWith("/worker/") && req.method === "POST") {
        const worker = store.all("workers").find((w) => equal(w.token, bearer));
        if (!worker) return reply(401, { error: "Unauthorized worker" });
        return reply(
          200,
          await workerAction(
            path.slice(8),
            JSON.parse((await requestBody(req)).toString("utf8")),
            worker,
          ),
        );
      }
      reply(404, { error: "Not found" });
    } catch (e) {
      logger.error("Request failed:", errorMessage(e));
      reply(errorNumber(e, "status") || 400, { error: errorMessage(e) });
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(config.port, config.bind, resolve);
  });
  const intervals = [
    setInterval(
      () => void tick().catch((e) => logger.error(errorMessage(e))),
      1000,
    ),
    setInterval(
      () => void publishTick().catch((e) => logger.error(errorMessage(e))),
      1000,
    ),
    setInterval(() => void audit(), config.auditIntervalMs),
  ];
  void audit();
  if (config.catchUp.enabled)
    for (const r of store.all("repos"))
      void catchUp(r.name).catch((e) =>
        logger.error("Catch-up deferred:", errorMessage(e)),
      );
  return {
    server,
    store,
    admin,
    close: async () => {
      closed = true;
      intervals.forEach(clearInterval);
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
      while (busy || publishing || auditing)
        await new Promise((r) => setTimeout(r, 50));
      store.close();
    },
  };
}
