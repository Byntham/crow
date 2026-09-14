import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { Delegation } from "../dist/lib/delegation.mjs";

const report = { summary: "No actionable findings.", findings: [] };

function controlledRun() {
  const calls = [];
  let active = 0,
    peak = 0;
  const run = async (options) => {
    active++;
    peak = Math.max(peak, active);
    let resolve, reject;
    const pending = new Promise((yes, no) => {
      resolve = yes;
      reject = no;
    });
    pending.catch(() => {});
    const abort = () =>
      reject(options.signal.reason || new Error("Interrupted"));
    options.signal.addEventListener("abort", abort, { once: true });
    const call = { ...options, resolve, reject };
    calls.push(call);
    try {
      await options.onSession(options.job.session || `session-${calls.length}`);
      if (options.signal.aborted) abort();
      return await pending;
    } finally {
      active--;
      options.signal.removeEventListener("abort", abort);
    }
  };
  return {
    run,
    calls,
    get active() {
      return active;
    },
    get peak() {
      return peak;
    },
  };
}

async function eventually(predicate) {
  const deadline = Date.now() + 2000;
  while (!(await predicate())) {
    if (Date.now() >= deadline)
      assert.fail("Delegation did not reach the expected state");
    await delay(5);
  }
}

async function fixture(t, subagents = { mode: "inherit", max: 2 }, run) {
  const root = await mkdtemp(join(tmpdir(), "crow-delegation-test-"));
  const context = {
    root,
    job: {
      id: "parent-review",
      repo: "owner/repository",
      number: 12,
      comparison: {
        head: "a".repeat(40),
        base: "b".repeat(40),
        target: "main",
      },
      settings: {
        model: "parent-model",
        effort: "high",
        codexHome: join(root, "codex"),
        subagents,
        retry: { mode: "fixed", count: 10, delayMs: 1 },
      },
    },
    source: {
      dir: join(root, "source"),
      head: "a".repeat(40),
      base: "b".repeat(40),
      target: "main",
    },
    guidance: { files: [{ path: "AGENTS.md", body: "Check access control." }] },
  };
  const controlled = controlledRun();
  const delegation = new Delegation(context, { run: run || controlled.run });
  await delegation.init();
  const instances = [delegation];
  t.after(async () => {
    for (const instance of instances) await instance.close();
    await rm(root, { recursive: true, force: true });
  });
  const status = (id) => delegation.call("review_task_status", { id });
  const persisted = (id) =>
    readFile(
      join(root, "reviews", context.job.id, "tasks", `${id}.json`),
      "utf8",
    ).then(JSON.parse);
  const reload = async () => {
    const next = new Delegation(context, { run: controlled.run });
    instances.push(next);
    await next.init();
    return next;
  };
  return { root, context, delegation, controlled, status, persisted, reload };
}

test("delegated review ceiling holds under simultaneous tool calls and frees a slot on completion", async (t) => {
  const f = await fixture(t);
  const starts = await Promise.allSettled(
    Array.from({ length: 6 }, (_, index) =>
      f.delegation.call("start_review_task", { task: `Inspect area ${index}` }),
    ),
  );
  assert.equal(
    starts.filter((result) => result.status === "fulfilled").length,
    2,
  );
  assert.equal(
    starts.filter((result) => result.status === "rejected").length,
    4,
  );
  await eventually(() => f.controlled.calls.length === 2);
  assert.equal(f.controlled.peak, 2);
  const first = starts.find(
    (result) =>
      result.status === "fulfilled" &&
      result.value.task === f.controlled.calls[0].task,
  ).value;
  f.controlled.calls[0].resolve(report);
  await eventually(
    async () => (await f.status(first.id)).state === "completed",
  );
  await f.delegation.call("start_review_task", {
    task: "Inspect a remaining area",
  });
  await eventually(() => f.controlled.calls.length === 3);
  assert.equal(f.controlled.peak, 2);
});

test("one available slot admits exactly one of two overlapping starts", async (t) => {
  const f = await fixture(t, { mode: "inherit", max: 1 });
  const starts = await Promise.allSettled([
    f.delegation.call("start_review_task", { task: "Inspect access control" }),
    f.delegation.call("start_review_task", { task: "Inspect error handling" }),
  ]);
  assert.equal(
    starts.filter((result) => result.status === "fulfilled").length,
    1,
  );
  assert.equal(
    starts.filter((result) => result.status === "rejected").length,
    1,
  );
  await eventually(() => f.controlled.calls.length === 1);
  assert.equal(f.controlled.peak, 1);
  assert.equal((await f.delegation.call("review_task_status", {})).length, 1);
});

for (const outcome of [
  "completed",
  "paused with session",
  "paused without session",
]) {
  test(`delegated ${outcome} state stays private until final persistence releases its slot`, async (t) => {
    const f = await fixture(t, { mode: "inherit", max: 1 });
    const result = Promise.withResolvers();
    const saving = Promise.withResolvers();
    const finishSave = Promise.withResolvers();
    result.promise.catch(() => {});
    f.delegation.run = async (options) => {
      if (outcome === "paused with session")
        await options.onSession("saved-session");
      return result.promise;
    };
    const save = f.delegation.save.bind(f.delegation);
    f.delegation.save = async (task) => {
      if (task.state === "completed" || task.state === "paused") {
        saving.resolve();
        await finishSave.promise;
      }
      await save(task);
    };
    try {
      const task = await f.delegation.call("start_review_task", {
        task: "Inspect finalization",
      });
      if (outcome === "completed") result.resolve(report);
      else result.reject(new Error("Provider unavailable"));
      await saving.promise;
      const pending = await f.status(task.id);
      assert.equal(pending.state, "running");
      assert.equal(pending.report, undefined);
      assert.equal(pending.canResume, false);
      assert.equal(pending.canRestart, false);
      assert.equal((await f.persisted(task.id)).state, "running");
      await assert.rejects(
        f.delegation.call("resume_review_task", { id: task.id }),
        /Only a paused/,
      );
      await assert.rejects(
        f.delegation.call("restart_review_task", { id: task.id }),
        /Only a paused/,
      );
      await assert.rejects(
        f.delegation.launch(f.delegation.require(task.id)),
        /still running or saving/,
      );
      finishSave.resolve();
      const final = await f.delegation.call("wait_review_task", {
        id: task.id,
      });
      assert.equal(
        final.state,
        outcome === "completed" ? "completed" : "paused",
      );
      assert.equal(f.delegation.active.size, 0);
      assert.equal((await f.persisted(task.id)).state, final.state);
      assert.equal(final.canResume, outcome === "paused with session");
      assert.equal(final.canRestart, outcome === "paused without session");
      if (outcome === "completed") assert.deepEqual(final.report, report);
    } finally {
      finishSave.resolve();
      result.resolve(report);
    }
  });
}

for (const mode of ["inherit", "configured"]) {
  test(`${mode} delegation fixes child settings and prevents nested delegation`, async (t) => {
    const subagents =
      mode === "inherit"
        ? { mode, max: 2 }
        : { mode, max: 2, model: "configured-child-model", effort: "medium" };
    const f = await fixture(t, subagents);
    await assert.rejects(
      f.delegation.call("start_review_task", {
        task: "Inspect authorization",
        model: "unapproved-model",
        effort: "low",
        subagents: { max: 99 },
      }),
      /controlled by Crow/,
    );
    assert.equal(f.controlled.calls.length, 0);
    await f.delegation.call("start_review_task", {
      task: "Inspect authorization",
    });
    await eventually(() => f.controlled.calls.length === 1);
    const child = f.controlled.calls[0];
    assert.equal(
      child.job.settings.model,
      mode === "inherit" ? "parent-model" : "configured-child-model",
    );
    assert.equal(
      child.job.settings.effort,
      mode === "inherit" ? "high" : "medium",
    );
    assert.equal(child.job.settings.subagents.max, 0);
    assert.equal(child.job.settings.detached, false);
    assert.equal(child.root, f.root);
    assert.deepEqual(child.source, f.context.source);
    assert.deepEqual(child.guidance, f.context.guidance);
    assert.equal(child.task, "Inspect authorization");
    assert.notEqual(child.job.id, f.context.job.id);
    assert.equal(child.job.parentId, f.context.job.id);
    assert.ok(child.job.taskId);
    assert.equal(f.context.job.settings.model, "parent-model");
    assert.equal(f.context.job.settings.subagents.max, 2);
  });
}

test("completed reports persist and can be read after restarting delegation without new inference", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect error handling",
  });
  await eventually(() => f.controlled.calls.length === 1);
  assert.equal((await f.status(task.id)).report, undefined);
  f.controlled.calls[0].resolve(report);
  const result = await f.delegation.call("wait_review_task", {
    id: task.id,
    timeoutMs: 1000,
  });
  assert.equal(result.state, "completed");
  assert.deepEqual(result.report, report);
  assert.deepEqual((await f.persisted(task.id)).report, report);
  await f.delegation.close();
  const next = await f.reload();
  assert.deepEqual(
    (await next.call("review_task_status", { id: task.id })).report,
    report,
  );
  assert.equal(f.controlled.calls.length, 1);
});

test("closing interrupts active work, saves its session, and explicit resume uses that session", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect concurrency",
  });
  await eventually(
    async () => (await f.persisted(task.id)).session === "session-1",
  );
  await f.delegation.close();
  assert.equal(f.controlled.active, 0);
  const saved = await f.persisted(task.id);
  assert.equal(saved.state, "paused");
  assert.equal(saved.session, "session-1");
  assert.equal(saved.report, undefined);
  const next = await f.reload();
  await next.call("resume_review_task", { id: task.id });
  await eventually(() => f.controlled.calls.length === 2);
  assert.equal(f.controlled.calls[1].job.session, "session-1");
  assert.equal(f.controlled.calls[1].job.id, f.controlled.calls[0].job.id);
  f.controlled.calls[1].resolve(report);
  assert.equal(
    (await next.call("wait_review_task", { id: task.id, timeoutMs: 1000 }))
      .state,
    "completed",
  );
});

test("failed child keeps its saved session but exposes no partial findings", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect payment handling",
  });
  await eventually(
    async () => (await f.persisted(task.id)).session === "session-1",
  );
  f.controlled.calls[0].reject(
    Object.assign(new Error("Provider temporarily unavailable"), {
      kind: "transient",
      report: { summary: "Partial", findings: [{ title: "Unverified" }] },
    }),
  );
  const result = await f.delegation.call("wait_review_task", {
    id: task.id,
    timeoutMs: 1000,
  });
  assert.notEqual(result.state, "running");
  assert.notEqual(result.state, "completed");
  assert.equal(result.canResume, true);
  assert.equal((await f.persisted(task.id)).session, "session-1");
  assert.equal(result.report, undefined);
  assert.equal((await f.persisted(task.id)).report, undefined);
});

test("persisted running tasks recover as paused and never restart without an explicit request", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect transaction boundaries",
  });
  await eventually(
    async () => (await f.persisted(task.id)).session === "session-1",
  );
  const beforeInterruption = await f.persisted(task.id);
  assert.equal(beforeInterruption.state, "running");
  await f.delegation.close();
  // Restore the last persisted state from before an abrupt parent exit.
  await writeFile(
    join(f.root, "reviews", f.context.job.id, "tasks", `${task.id}.json`),
    JSON.stringify(beforeInterruption),
  );
  const next = await f.reload();
  const recovered = await next.call("review_task_status", { id: task.id });
  assert.equal(recovered.state, "paused");
  assert.equal(recovered.canResume, true);
  assert.equal((await f.persisted(task.id)).state, "paused");
  assert.equal(f.controlled.calls.length, 1);
});

test("saved delegated tasks reject retry modes with a non-string representation", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect transaction boundaries",
  });
  await eventually(
    async () => (await f.persisted(task.id)).session === "session-1",
  );
  await f.delegation.close();
  const saved = await f.persisted(task.id);
  saved.settings.retry.mode = ["fixed"];
  await writeFile(
    join(f.root, "reviews", f.context.job.id, "tasks", `${task.id}.json`),
    JSON.stringify(saved),
  );
  await assert.rejects(f.reload(), /Invalid saved delegated task/);
  assert.equal(f.controlled.calls.length, 1);
});

test("interruption before a session is saved requires an explicit new task", async (t) => {
  let starts = 0;
  const f = await fixture(t, { mode: "inherit", max: 1 }, async () => {
    starts++;
    throw new Error("Provider failed before starting a session");
  });
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect API routing",
  });
  const result = await f.delegation.call("wait_review_task", {
    id: task.id,
    timeoutMs: 1000,
  });
  assert.equal(result.state, "paused");
  assert.equal(result.canResume, false);
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: task.id }),
    /No usable saved session/,
  );
  assert.equal(starts, 1);
});

test("task lookup rejects paths and unknown ids without touching other review state", async (t) => {
  const f = await fixture(t);
  await assert.rejects(
    f.delegation.call("review_task_status", { id: "../../other-review" }),
  );
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: "unknown-task" }),
  );
});

async function nextAttempt(task) {
  await delay(Math.max(0, (task.nextAttemptAt || 0) - Date.now()) + 2);
}

test("explicit restart preserves a failed task and links to a fresh replacement with the same settings", async (t) => {
  let starts = 0;
  const f = await fixture(
    t,
    { mode: "configured", max: 1, model: "child-model", effort: "medium" },
    async (options) => {
      starts++;
      if (starts === 1)
        throw new Error("Connection failed before session creation");
      assert.equal(options.job.settings.model, "child-model");
      assert.equal(options.job.settings.effort, "medium");
      assert.equal(options.task, "Inspect locks");
      assert.equal(options.job.session, null);
      return report;
    },
  );
  const original = await f.delegation.call("start_review_task", {
    task: "Inspect locks",
  });
  const failed = await f.delegation.call("wait_review_task", {
    id: original.id,
  });
  await nextAttempt(failed);
  const replacement = await f.delegation.call("restart_review_task", {
    id: original.id,
  });
  assert.notEqual(replacement.id, original.id);
  assert.equal((await f.status(original.id)).state, "superseded");
  assert.equal((await f.status(original.id)).replacement, replacement.id);
  assert.equal((await f.persisted(original.id)).replacement, replacement.id);
  assert.equal(
    (await f.delegation.call("wait_review_task", { id: replacement.id })).state,
    "completed",
  );
  assert.equal((await f.persisted(replacement.id)).replaces, original.id);
  await assert.rejects(
    f.delegation.call("restart_review_task", { id: original.id }),
    /Only a paused/,
  );
  assert.equal(starts, 2);
});

test("consecutive failures exhaust the configured resume budget and warnings do not reset it", async (t) => {
  const f = await fixture(t);
  f.context.job.settings.retry.count = 2;
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect retries",
  });
  for (let attempt = 0; attempt < 3; attempt++) {
    await eventually(() => f.controlled.calls.length === attempt + 1);
    await f.controlled.calls[attempt].onProgress({
      type: "warning",
      message: "Using cached model list",
    });
    f.controlled.calls[attempt].reject(new Error("Provider unavailable"));
    const failed = await f.delegation.call("wait_review_task", { id: task.id });
    assert.equal((await f.persisted(task.id)).consecutiveFailures, attempt + 1);
    if (attempt < 2) {
      await nextAttempt(failed);
      await f.delegation.call("resume_review_task", { id: task.id });
    }
  }
  assert.equal((await f.status(task.id)).requiresOperator, true);
  assert.equal((await f.status(task.id)).canResume, false);
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: task.id }),
    /requires operator/,
  );
  assert.equal(f.controlled.calls.length, 3);
});

test("actual inspection progress resets consecutive failures before a later outage", async (t) => {
  const f = await fixture(t);
  f.context.job.settings.retry.count = 1;
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect queue boundaries",
  });
  await eventually(() => f.controlled.calls.length === 1);
  f.controlled.calls[0].reject(new Error("Provider unavailable"));
  await nextAttempt(
    await f.delegation.call("wait_review_task", { id: task.id }),
  );
  await f.delegation.call("resume_review_task", { id: task.id });
  await eventually(() => f.controlled.calls.length === 2);
  await f.controlled.calls[1].onProgress({ type: "mcp_tool_call" });
  f.controlled.calls[1].reject(new Error("Provider unavailable again"));
  const failed = await f.delegation.call("wait_review_task", { id: task.id });
  assert.equal((await f.persisted(task.id)).consecutiveFailures, 1);
  assert.equal(failed.requiresOperator, false);
});

test("authentication diagnostics stay local and model-visible failures require operator action", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect authentication",
  });
  await eventually(() => f.controlled.calls.length === 1);
  f.controlled.calls[0].reject(
    Object.assign(new Error("refresh token expired: PRIVATE_AUTH_TOKEN"), {
      kind: "auth",
    }),
  );
  const failed = await f.delegation.call("wait_review_task", { id: task.id });
  assert.equal(failed.errorKind, "auth");
  assert.equal(failed.requiresOperator, true);
  assert.ok(!JSON.stringify(failed).includes("PRIVATE_AUTH_TOKEN"));
  assert.ok(
    !JSON.stringify(await f.delegation.call("review_task_status", {})).includes(
      "PRIVATE_AUTH_TOKEN",
    ),
  );
  assert.match((await f.persisted(task.id)).diagnostic, /PRIVATE_AUTH_TOKEN/);
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: task.id }),
    /operator action/,
  );
});

test("default retry delay is five seconds and a provider-requested longer wait takes precedence", async (t) => {
  const f = await fixture(t);
  delete f.context.job.settings.retry;
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect timeout handling",
  });
  await eventually(() => f.controlled.calls.length === 1);
  const beforeFailure = Date.now();
  f.controlled.calls[0].reject(new Error("Provider temporarily unavailable"));
  const failed = await f.delegation.call("wait_review_task", { id: task.id });
  assert.ok(failed.nextAttemptAt >= beforeFailure + 5000);
  assert.ok(failed.nextAttemptAt <= Date.now() + 5000);
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: task.id }),
    /retry is delayed/,
  );
  const second = await f.delegation.call("start_review_task", {
    task: "Inspect cancellation",
  });
  await eventually(() => f.controlled.calls.length === 2);
  f.controlled.calls[1].reject(
    Object.assign(new Error("Provider unavailable"), {
      kind: "transient",
      retryAfter: 60000,
    }),
  );
  assert.ok(
    (await f.delegation.call("wait_review_task", { id: second.id }))
      .nextAttemptAt >=
      beforeFailure + 60000,
  );
});

test("restarting tasks without sessions retains their failure budget", async (t) => {
  let starts = 0;
  const f = await fixture(t, { mode: "inherit", max: 1 }, async () => {
    starts++;
    throw new Error("Provider failed before session creation");
  });
  f.context.job.settings.retry.count = 1;
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect event delivery",
  });
  await nextAttempt(
    await f.delegation.call("wait_review_task", { id: task.id }),
  );
  const replacement = await f.delegation.call("restart_review_task", {
    id: task.id,
  });
  const failed = await f.delegation.call("wait_review_task", {
    id: replacement.id,
  });
  assert.equal(failed.requiresOperator, true);
  assert.equal((await f.persisted(replacement.id)).consecutiveFailures, 2);
  await assert.rejects(
    f.delegation.call("restart_review_task", { id: replacement.id }),
    /operator action/,
  );
  assert.equal(starts, 2);
});

test("invalid final reports permit at most two corrections despite output progress events", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect report generation",
  });
  for (let attempt = 0; attempt < 3; attempt++) {
    await eventually(() => f.controlled.calls.length === attempt + 1);
    await f.controlled.calls[attempt].onProgress({ type: "agent_message" });
    f.controlled.calls[attempt].reject(
      Object.assign(new Error("Invalid final review response"), {
        kind: "output",
      }),
    );
    const failed = await f.delegation.call("wait_review_task", { id: task.id });
    if (attempt < 2) {
      await nextAttempt(failed);
      await f.delegation.call("resume_review_task", { id: task.id });
    }
  }
  assert.equal((await f.persisted(task.id)).outputFailures, 3);
  assert.equal((await f.status(task.id)).requiresOperator, true);
  await assert.rejects(
    f.delegation.call("resume_review_task", { id: task.id }),
    /operator action/,
  );
});

for (const kind of ["auth", "quota", "transient"]) {
  test(`manual resume epoch clears ${kind} gates once while retaining sessions and waiting for explicit task resume`, async (t) => {
    const f = await fixture(t);
    f.context.job.settings.retry.count = 0;
    const task = await f.delegation.call("start_review_task", {
      task: "Inspect authorization boundaries",
    });
    await eventually(() => f.controlled.calls.length === 1);
    f.controlled.calls[0].reject(
      Object.assign(new Error("Saved provider diagnostic"), { kind }),
    );
    assert.equal(
      (await f.delegation.call("wait_review_task", { id: task.id }))
        .requiresOperator,
      true,
    );
    await f.delegation.close();
    f.context.job.resumeEpoch = 1;
    const resumed = await f.reload();
    const reset = await f.persisted(task.id);
    assert.equal(reset.operatorResumeEpoch, 1);
    assert.equal(reset.requiresOperator, false);
    assert.equal(reset.consecutiveFailures, 0);
    assert.equal(reset.outputFailures, 0);
    assert.equal(reset.nextAttemptAt, undefined);
    assert.equal(reset.session, "session-1");
    assert.equal(reset.diagnostic, "Saved provider diagnostic");
    assert.equal(reset.task, "Inspect authorization boundaries");
    assert.equal(reset.state, "paused");
    assert.equal(f.controlled.calls.length, 1);
    await resumed.call("resume_review_task", { id: task.id });
    await eventually(() => f.controlled.calls.length === 2);
    assert.equal(f.controlled.calls[1].job.session, "session-1");
    f.controlled.calls[1].reject(
      Object.assign(new Error("Provider still unavailable"), { kind }),
    );
    const failedAgain = await resumed.call("wait_review_task", { id: task.id });
    assert.equal(failedAgain.requiresOperator, true);
    await resumed.close();
    const automaticRetry = await f.reload();
    assert.equal(
      (await automaticRetry.call("review_task_status", { id: task.id }))
        .requiresOperator,
      true,
    );
    assert.equal((await f.persisted(task.id)).consecutiveFailures, 1);
    await assert.rejects(
      automaticRetry.call("resume_review_task", { id: task.id }),
      /operator action/,
    );
    assert.equal(f.controlled.calls.length, 2);
  });
}

for (const mode of ["inherit", "configured"]) {
  test(`manual resume records intentional ${mode} model changes while automatic retries retain captured settings`, async (t) => {
    const f = await fixture(t, {
      mode,
      max: 1,
      model: "original-child-model",
      effort: "medium",
    });
    const task = await f.delegation.call("start_review_task", {
      task: "Inspect persistence",
    });
    await eventually(
      async () => (await f.persisted(task.id)).session === "session-1",
    );
    await f.delegation.close();
    const original = await f.persisted(task.id);
    f.context.job.settings.model = "new-parent-model";
    f.context.job.settings.effort = "low";
    f.context.job.settings.subagents.model = "new-child-model";
    f.context.job.settings.subagents.effort = "high";
    const automaticRetry = await f.reload();
    const unchanged = await f.persisted(task.id);
    assert.equal(unchanged.settings.model, original.settings.model);
    assert.equal(unchanged.settings.effort, original.settings.effort);
    assert.equal(unchanged.settingsHistory, undefined);
    await automaticRetry.close();
    f.context.job.resumeEpoch = 1;
    const manualResume = await f.reload();
    const changed = await f.persisted(task.id);
    const expected =
      mode === "inherit"
        ? { model: "new-parent-model", effort: "low" }
        : { model: "new-child-model", effort: "high" };
    assert.equal(changed.settings.model, expected.model);
    assert.equal(changed.settings.effort, expected.effort);
    assert.equal(changed.session, "session-1");
    assert.equal(changed.settingsHistory.length, 1);
    assert.deepEqual(changed.settingsHistory[0].previous, {
      model: original.settings.model,
      effort: original.settings.effort,
    });
    assert.deepEqual(changed.settingsHistory[0].next, expected);
    assert.equal(changed.settingsHistory[0].resumeEpoch, 1);
    assert.equal(f.controlled.calls.length, 1);
    await manualResume.close();
    await f.reload();
    assert.equal((await f.persisted(task.id)).settingsHistory.length, 1);
  });
}

test("tasks created during a manual resume retain exhausted budgets on later automatic retries", async (t) => {
  const f = await fixture(t);
  await f.delegation.close();
  f.context.job.resumeEpoch = 3;
  f.context.job.settings.retry.count = 0;
  const resumed = await f.reload();
  const task = await resumed.call("start_review_task", {
    task: "Inspect newly delegated work",
  });
  await eventually(() => f.controlled.calls.length === 1);
  f.controlled.calls[0].reject(new Error("Provider unavailable"));
  await resumed.call("wait_review_task", { id: task.id });
  assert.equal((await f.persisted(task.id)).operatorResumeEpoch, 3);
  await resumed.close();
  const next = await f.reload();
  assert.equal(
    (await next.call("review_task_status", { id: task.id })).requiresOperator,
    true,
  );
});

test("manual resume retains completed report model metadata", async (t) => {
  const f = await fixture(t);
  const task = await f.delegation.call("start_review_task", {
    task: "Inspect completion metadata",
  });
  await eventually(() => f.controlled.calls.length === 1);
  f.controlled.calls[0].resolve(report);
  await f.delegation.call("wait_review_task", { id: task.id });
  await f.delegation.close();
  f.context.job.resumeEpoch = 1;
  f.context.job.settings.model = "new-model";
  await f.reload();
  const completed = await f.persisted(task.id);
  assert.equal(completed.state, "completed");
  assert.equal(completed.settings.model, "parent-model");
  assert.deepEqual(completed.report, report);
  assert.equal(completed.settingsHistory, undefined);
});
