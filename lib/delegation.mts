import { readdir, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { atomic, json, id } from "./util.mjs";
import { runReview, classifyError } from "./provider.mjs";
import type { RunReviewOptions } from "./provider.mjs";
import type { ReviewReport } from "./types.mjs";

type TaskState = "queued" | "running" | "paused" | "completed" | "superseded";
type TaskSettings = RunReviewOptions["job"]["settings"];
interface DelegatedTask {
  id: string;
  task: string;
  settings: TaskSettings;
  state: TaskState;
  createdAt: string;
  updatedAt?: string;
  operatorResumeEpoch?: number;
  session?: string;
  report?: ReviewReport;
  error?: string;
  errorKind?: string;
  diagnostic?: string;
  requiresOperator?: boolean;
  consecutiveFailures?: number;
  outputFailures?: number;
  nextAttemptAt?: number;
  replaces?: string;
  replacement?: string;
  settingsHistory?: Array<{
    resumeEpoch: number;
    changedAt: string;
    previous: Pick<TaskSettings, "model" | "effort">;
    next: Pick<TaskSettings, "model" | "effort">;
  }>;
}
interface ActiveTask {
  controller: AbortController;
  promise: Promise<void> | null;
}
function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function selection(value: unknown): boolean {
  return (
    record(value) &&
    [value.model, value.effort].every(
      (field) => field === null || typeof field === "string",
    )
  );
}
/** Saved task files are a versioned internal format; reject malformed records before reuse. */
function savedTask(value: unknown): value is DelegatedTask {
  if (
    !record(value) ||
    typeof value.id !== "string" ||
    typeof value.task !== "string" ||
    typeof value.createdAt !== "string" ||
    typeof value.state !== "string" ||
    !["queued", "running", "paused", "completed", "superseded"].includes(
      value.state,
    )
  )
    return false;
  const settings = value.settings;
  if (
    !record(settings) ||
    !selection(settings) ||
    typeof settings.timeoutMs !== "number" ||
    !record(settings.subagents) ||
    settings.subagents.mode !== "inherit" ||
    typeof settings.subagents.max !== "number" ||
    !record(settings.retry) ||
    typeof settings.retry.mode !== "string" ||
    !["fixed", "progressive"].includes(settings.retry.mode) ||
    typeof settings.retry.count !== "number" ||
    typeof settings.retry.delayMs !== "number"
  )
    return false;
  for (const key of ["codex", "codexHome"])
    if (settings[key] !== undefined && typeof settings[key] !== "string")
      return false;
  for (const key of [
    "session",
    "error",
    "errorKind",
    "diagnostic",
    "replaces",
    "replacement",
    "updatedAt",
  ])
    if (value[key] !== undefined && typeof value[key] !== "string")
      return false;
  for (const key of [
    "operatorResumeEpoch",
    "consecutiveFailures",
    "outputFailures",
    "nextAttemptAt",
  ])
    if (value[key] !== undefined && typeof value[key] !== "number")
      return false;
  if (
    value.requiresOperator !== undefined &&
    typeof value.requiresOperator !== "boolean"
  )
    return false;
  if (
    value.settingsHistory !== undefined &&
    (!Array.isArray(value.settingsHistory) ||
      !value.settingsHistory.every(
        (change: unknown) =>
          record(change) &&
          typeof change.resumeEpoch === "number" &&
          typeof change.changedAt === "string" &&
          selection(change.previous) &&
          selection(change.next),
      ))
  )
    return false;
  if (value.report !== undefined) {
    if (
      !record(value.report) ||
      typeof value.report.summary !== "string" ||
      !Array.isArray(value.report.findings)
    )
      return false;
    if (
      !value.report.findings.every(
        (finding: unknown) =>
          record(finding) &&
          [finding.id, finding.title, finding.body, finding.path].every(
            (field) => typeof field === "string",
          ) &&
          typeof finding.line === "number" &&
          typeof finding.severity === "string" &&
          ["critical", "high", "medium", "low"].includes(finding.severity),
      )
    )
      return false;
  }
  return true;
}

export const delegationTools = [
  {
    name: "start_review_task",
    description:
      "Delegate a bounded independent code-inspection task. Model, reasoning, permissions and concurrency are fixed by Crow. Returns a task ID immediately. Await all relevant task results before returning the consolidated report.",
    properties: { task: { type: "string" } },
    required: ["task"],
  },
  {
    name: "review_task_status",
    description:
      "Read saved delegated task states and complete reports. On resumed reviews, list tasks to recover prior results. Paused tasks need explicit resume.",
    properties: { id: { type: "string" } },
  },
  {
    name: "resume_review_task",
    description:
      "Resume a paused delegated task from its saved provider session after nextAttemptAt. Retry limits are enforced by Crow. No model override is accepted.",
    properties: { id: { type: "string" } },
    required: ["id"],
  },
  {
    name: "restart_review_task",
    description:
      "Explicitly replace a paused task that has no saved session. Retains its fixed settings, task and failure budget. Follow the replacement ID for results. Cannot bypass retry limits.",
    properties: { id: { type: "string" } },
    required: ["id"],
  },
  {
    name: "wait_review_task",
    description:
      "Wait briefly for a delegated task, then return its state and complete report when available.",
    properties: {
      id: { type: "string" },
      timeoutMs: { type: "integer", minimum: 1, maximum: 30000 },
    },
    required: ["id"],
  },
];
const progressTypes = new Set(["mcp_tool_call", "agent_message", "reasoning"]);
const retryPolicy = (task: DelegatedTask) => ({
  mode: "fixed",
  count: 10,
  delayMs: 5000,
  ...(task.settings.retry as Partial<TaskSettings["retry"]>),
});
function failureMessage(task: DelegatedTask) {
  if (task.requiresOperator)
    return "Delegated review paused. Operator action is required before further attempts.";
  if (task.errorKind === "interrupted")
    return "Delegated review interrupted. Resume its saved session when available.";
  if (task.errorKind === "output")
    return "Delegated review returned an invalid final report. Resume to correct it after nextAttemptAt.";
  return task.session
    ? "Delegated review failed. Resume its saved session after nextAttemptAt."
    : "Delegated review failed before saving a session. Use restart_review_task after nextAttemptAt.";
}

export class Delegation {
  context: RunReviewOptions;
  run: typeof runReview;
  tasks: Map<string, DelegatedTask>;
  active: Map<string, ActiveTask>;
  stopping: boolean;
  epoch: number;
  dir: string;
  constructor(
    context: RunReviewOptions,
    { run = runReview }: { run?: typeof runReview } = {},
  ) {
    this.context = context;
    this.run = run;
    this.tasks = new Map();
    this.active = new Map();
    this.stopping = false;
    this.epoch =
      Number.isSafeInteger(context.job.resumeEpoch) &&
      (context.job.resumeEpoch ?? 0) > 0
        ? (context.job.resumeEpoch ?? 0)
        : 0;
    this.dir = join(context.root, "reviews", context.job.id, "tasks");
  }
  async init() {
    await mkdir(this.dir, { recursive: true, mode: 0o700 });
    for (const name of await readdir(this.dir)) {
      if (!/^[a-f0-9]+\.json$/.test(name)) continue;
      const task: unknown = await json(join(this.dir, name));
      if (!record(task) || !task.id) continue;
      if (!savedTask(task))
        throw new Error(`Invalid saved delegated task: ${name}`);
      let changed = false;
      if (task.state === "running") {
        task.state = "paused";
        task.errorKind = "interrupted";
        changed = true;
      }
      if (this.epoch > (task.operatorResumeEpoch || 0)) {
        task.operatorResumeEpoch = this.epoch;
        changed = true;
        if (task.state === "paused" || task.state === "queued") {
          task.requiresOperator = false;
          task.consecutiveFailures = 0;
          task.outputFailures = 0;
          delete task.nextAttemptAt;
          const parent = this.context.job.settings,
            sub = parent.subagents || { mode: "inherit" };
          const next = {
            model: sub.mode === "configured" ? sub.model : parent.model,
            effort: sub.mode === "configured" ? sub.effort : parent.effort,
          };
          if (
            task.settings.model !== next.model ||
            task.settings.effort !== next.effort
          ) {
            task.settingsHistory ??= [];
            task.settingsHistory.push({
              resumeEpoch: this.epoch,
              changedAt: new Date().toISOString(),
              previous: {
                model: task.settings.model,
                effort: task.settings.effort,
              },
              next,
            });
            Object.assign(task.settings, next);
          }
        }
      }
      if (changed) await this.save(task);
      this.tasks.set(task.id, task);
    }
    return this;
  }
  async save(task: DelegatedTask) {
    task.updatedAt = new Date().toISOString();
    await atomic(join(this.dir, `${task.id}.json`), task);
  }
  view(task: DelegatedTask) {
    // Final state becomes visible only after persistence releases the slot.
    const active = this.active.has(task.id);
    return {
      id: task.id,
      state: active ? "running" : task.state,
      task: task.task,
      model: task.settings.model,
      effort: task.settings.effort,
      ...(!active && task.report ? { report: task.report } : {}),
      ...(!active && (task.errorKind || task.error)
        ? {
            errorKind: task.errorKind || "transient",
            error: failureMessage(task),
          }
        : {}),
      ...(task.replacement ? { replacement: task.replacement } : {}),
      ...(!active && task.nextAttemptAt
        ? { nextAttemptAt: task.nextAttemptAt }
        : {}),
      requiresOperator: !active && !!task.requiresOperator,
      canResume:
        !active &&
        task.state === "paused" &&
        !!task.session &&
        !task.requiresOperator,
      canRestart:
        !active &&
        task.state === "paused" &&
        !task.session &&
        !task.requiresOperator,
    };
  }
  require(id: unknown) {
    if (typeof id !== "string") throw new Error("Unknown delegated task ID");
    const task = this.tasks.get(id);
    if (!task) throw new Error("Unknown delegated task ID");
    return task;
  }
  async checkRetry(task: DelegatedTask) {
    if (
      (task.consecutiveFailures || 0) > retryPolicy(task).count ||
      (task.outputFailures || 0) > 2
    )
      task.requiresOperator = true;
    if (task.requiresOperator) {
      await this.save(task);
      throw new Error(
        "Delegated task requires operator action; automatic attempts are exhausted or unavailable.",
      );
    }
    if ((task.nextAttemptAt || 0) > Date.now())
      throw new Error(
        `Delegated task retry is delayed until ${new Date(task.nextAttemptAt ?? 0).toISOString()}.`,
      );
  }
  async launch(task: DelegatedTask, beforeRun?: () => Promise<void>) {
    if (this.stopping) throw new Error("Review is stopping");
    if (this.active.has(task.id))
      throw new Error("Delegated task is still running or saving its result");
    const max = this.context.job.settings.subagents?.max ?? 8;
    if (this.active.size >= max)
      throw new Error(
        `The ${max} concurrent delegated-task limit is reached. Wait for a task to finish.`,
      );
    task.state = "running";
    delete task.error;
    delete task.errorKind;
    delete task.nextAttemptAt;
    // Reserve synchronously so overlapping MCP calls cannot exceed the ceiling.
    const controller = new AbortController(),
      entry: ActiveTask = { controller, promise: null };
    this.active.set(task.id, entry);
    const starting = (async () => {
      await this.save(task);
      if (beforeRun) await beforeRun();
    })();
    const child = {
      ...this.context.job,
      id: `${this.context.job.id}_${task.id}`,
      parentId: this.context.job.id,
      taskId: task.id,
      session: task.session || null,
      settings: { ...task.settings, detached: false },
      delegated: true,
    };
    entry.promise = (async () => {
      try {
        await starting;
        if (controller.signal.aborted) throw controller.signal.reason;
        const report = await this.run({
          ...this.context,
          job: child,
          task: task.task,
          signal: controller.signal,
          onSession: async (session) => {
            task.session = session;
            await this.save(task);
          },
          onProgress: async (event) => {
            if (progressTypes.has(event.type)) {
              task.consecutiveFailures = 0;
              await this.save(task);
            }
          },
        });
        task.report = report;
        task.state = "completed";
        task.consecutiveFailures = 0;
        task.outputFailures = 0;
        task.requiresOperator = false;
      } catch (e) {
        task.state = "paused";
        delete task.report;
        if (controller.signal.aborted) {
          task.errorKind = "interrupted";
        } else {
          const failure = classifyError(e),
            retry = retryPolicy(task);
          task.errorKind = [
            "auth",
            "quota",
            "config",
            "restart",
            "output",
            "transient",
          ].includes(failure.kind)
            ? failure.kind
            : "transient";
          task.diagnostic = failure.message;
          task.consecutiveFailures = (task.consecutiveFailures || 0) + 1;
          if (task.errorKind === "output")
            task.outputFailures = (task.outputFailures || 0) + 1;
          task.requiresOperator =
            ["auth", "quota", "config", "restart"].includes(task.errorKind) ||
            task.consecutiveFailures > retry.count ||
            (task.outputFailures || 0) > 2;
          const schedule = [5000, 15000, 30000, 60000, 120000, 300000];
          const nominal =
            retry.mode === "progressive"
              ? schedule[
                  Math.min(task.consecutiveFailures - 1, schedule.length - 1)
                ]
              : retry.delayMs;
          task.nextAttemptAt =
            Date.now() +
            Math.max(
              nominal,
              typeof failure.retryAfter === "number" &&
                Number.isFinite(failure.retryAfter)
                ? failure.retryAfter
                : 0,
            );
        }
      } finally {
        try {
          await this.save(task);
        } finally {
          this.active.delete(task.id);
        }
      }
    })();
    entry.promise.catch(() => {});
    await starting;
    return this.view(task);
  }
  async call(name: string, input: unknown = {}) {
    const args = record(input) ? input : null;
    const definition = delegationTools.find((t) => t.name === name);
    if (!definition) throw new Error("Unknown delegation tool");
    if (
      !args ||
      typeof args !== "object" ||
      Array.isArray(args) ||
      Object.keys(args).some((k) => !Object.hasOwn(definition.properties, k))
    )
      throw new Error(
        "Unsupported delegated-task arguments; model and reasoning are controlled by Crow",
      );
    if (name === "start_review_task") {
      if (
        typeof args.task !== "string" ||
        !args.task.trim() ||
        args.task.length > 16000
      )
        throw new Error("A delegated task needs 1–16000 characters");
      const parent = this.context.job.settings,
        sub = parent.subagents || { mode: "inherit", max: 8 };
      const settings: TaskSettings = {
        codex: parent.codex,
        codexHome: parent.codexHome,
        model: sub.mode === "configured" ? sub.model : parent.model,
        effort: sub.mode === "configured" ? sub.effort : parent.effort,
        timeoutMs: parent.timeoutMs || 0,
        retry: parent.retry,
        subagents: { mode: "inherit", max: 0 },
      };
      const task: DelegatedTask = {
        id: id(),
        task: args.task,
        settings,
        state: "queued",
        createdAt: new Date().toISOString(),
        operatorResumeEpoch: this.epoch,
      };
      this.tasks.set(task.id, task);
      try {
        return await this.launch(task);
      } catch (e) {
        this.tasks.delete(task.id);
        throw e;
      }
    }
    if (name === "review_task_status")
      return args.id
        ? this.view(this.require(args.id))
        : [...this.tasks.values()].map((t) => this.view(t));
    const task = this.require(args.id);
    if (name === "resume_review_task") {
      if (this.active.has(task.id) || task.state !== "paused")
        throw new Error("Only a paused delegated task can resume");
      if (!task.session)
        throw new Error("No usable saved session; use restart_review_task");
      await this.checkRetry(task);
      // checkRetry may persist an exhausted budget; recheck after its await.
      if (this.active.has(task.id) || task.state !== "paused")
        throw new Error("Only a paused delegated task can resume");
      return this.launch(task);
    }
    if (name === "restart_review_task") {
      if (this.active.has(task.id) || task.state !== "paused" || task.session)
        throw new Error(
          "Only a paused delegated task without a saved session can restart",
        );
      await this.checkRetry(task);
      if (this.active.has(task.id) || task.state !== "paused")
        throw new Error("Only a paused delegated task can restart");
      const replacement: DelegatedTask = {
        id: id(),
        task: task.task,
        settings: structuredClone(task.settings),
        state: "queued",
        createdAt: new Date().toISOString(),
        consecutiveFailures: task.consecutiveFailures || 0,
        outputFailures: task.outputFailures || 0,
        replaces: task.id,
        operatorResumeEpoch: this.epoch,
      };
      task.state = "superseded";
      task.replacement = replacement.id;
      this.tasks.set(replacement.id, replacement);
      try {
        return await this.launch(replacement, () => this.save(task));
      } catch (e) {
        task.state = "paused";
        delete task.replacement;
        this.tasks.delete(replacement.id);
        await this.save(task);
        throw e;
      }
    }
    if (name === "wait_review_task") {
      const ms = args.timeoutMs ?? 1000;
      if (
        typeof ms !== "number" ||
        !Number.isInteger(ms) ||
        ms < 1 ||
        ms > 30000
      )
        throw new Error("Wait must be between 1 and 30000 milliseconds");
      const pending = this.active.get(task.id)?.promise;
      if (pending) {
        let timer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([
          pending,
          new Promise((r) => (timer = setTimeout(r, ms))),
        ]);
        clearTimeout(timer);
      }
      return this.view(task);
    }
  }
  async close() {
    this.stopping = true;
    const active = [...this.active.values()];
    for (const e of active)
      e.controller.abort(new Error("Parent review stopped"));
    await Promise.allSettled(active.map((e) => e.promise));
  }
}
