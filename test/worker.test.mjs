import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
  symlink,
  stat,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHmac } from "node:crypto";
import { startWorker, workerRequest } from "../dist/lib/worker.mjs";
import { startService } from "../dist/lib/service.mjs";
import { defaults } from "../dist/lib/config.mjs";
import { processRun, sleep, atomic, json, hash } from "../dist/lib/util.mjs";
import { metadata, inlineComments } from "../dist/lib/report.mjs";
import { publicationPatch } from "../dist/lib/inspection.mjs";

const session = "12345678-1234-1234-1234-123456789abc";
const report = {
  summary: "Inspected the change. No actionable findings.",
  findings: [],
};
const quiet = { error() {} };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
async function until(fn, message = "condition", timeout = 8000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const value = await fn();
    if (value) return value;
    await sleep(10);
  }
  throw new Error(`Timed out waiting for ${message}`);
}
async function local(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-worker-"));
  const fixture = {};
  t.after(async () => {
    await fixture.beforeClose?.();
    await fixture.worker?.close();
    await fixture.service?.close();
    await rm(root, { recursive: true, force: true });
  });
  const config = defaults(root);
  config.worker.concurrency = 1;
  config.worker.model = "reported-model";
  config.worker.effort = "high";
  const source = {
    dir: root,
    head: "a".repeat(40),
    base: "b".repeat(40),
    target: "main",
    targetSha: "b".repeat(40),
  };
  const job = {
    id: "job1",
    repo: "owner/project",
    number: 1,
    head: source.head,
    target: "main",
    lease: "lease1",
    session: null,
    settings: {},
  };
  return Object.assign(fixture, { root, config, source, job });
}
function workerOptions(fixture, extra = {}) {
  return {
    prepare: async () => fixture.source,
    readGuidance: async () => ({ files: [], fingerprint: hash([]) }),
    readDiff: async () => "",
    logger: quiet,
    heartbeatMs: 10,
    pollMs: 10,
    reconnectMs: 10,
    ...extra,
  };
}

test("worker retransmits a saved report after transport failure without repeating inference", async (t) => {
  const f = await local(t),
    sent = [];
  let next = { job: f.job },
    reviews = 0,
    sends = 0;
  const worker = (f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_config, action, body) => {
        sent.push({ action, body });
        if (action === "next") {
          const work = next;
          next = null;
          return work;
        }
        if (action === "report" && ++sends === 1)
          throw new Error("Lost response");
        if (action === "failed")
          next = { job: { ...f.job, lease: "lease2", session } };
        return { ok: true };
      },
      review: async ({ onSession, job }) => {
        reviews++;
        assert.equal(job.settings.codexHome, f.config.worker.codexHome);
        await onSession(session);
        return report;
      },
    }),
  ));
  await until(() => sends === 2, "saved report retransmission");
  await worker.drain();
  assert.equal(reviews, 1);
  assert.equal(
    (await json(join(f.root, "reports/job1.json"))).head,
    f.source.head,
  );
  const advertised = sent.find((x) => x.action === "next").body.defaults;
  assert.equal(advertised.model, "reported-model");
  for (const key of ["token", "codex", "codexHome", "id"])
    assert.equal(key in advertised, false);
});

test("worker publishes completed findings from a PR larger than 16 MiB without retrying inference", async (t) => {
  const f = await local(t);
  const git = (args) =>
    processRun("git", [
      "-C",
      f.root,
      "-c",
      "user.name=Fixture",
      "-c",
      "user.email=fixture@example.test",
      ...args,
    ]);
  await git(["init", "-b", "main"]);
  await git(["commit", "--allow-empty", "-m", "base"]);
  f.source.base = (await git(["rev-parse", "HEAD"])).stdout.trim();
  f.source.targetSha = f.source.base;
  await writeFile(
    join(f.root, "a-large.txt"),
    "x".repeat(17 * 1024 * 1024) + "\n",
  );
  await writeFile(join(f.root, "z-later.txt"), "late change\n");
  await git(["add", "a-large.txt", "z-later.txt"]);
  await git(["commit", "-m", "large PR"]);
  f.source.head = (await git(["rev-parse", "HEAD"])).stdout.trim();
  f.job.head = f.source.head;
  const reviewed = {
    summary: "Reviewed the complete comparison.",
    findings: ["a-large.txt", "z-later.txt"].map((path) => ({
      path,
      line: 1,
      severity: "high",
      title: "Actionable issue",
      body: "Fix this issue.",
    })),
  };
  let next = { job: f.job },
    published,
    reviews = 0,
    failures = 0;
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      readDiff: publicationPatch,
      request: async (_, action, body) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "report") published = body;
        if (action === "failed") failures++;
        return { ok: true };
      },
      review: async () => {
        reviews++;
        return reviewed;
      },
    }),
  );
  await until(() => published, "large PR publication");
  await f.worker.drain();
  assert.equal(reviews, 1);
  assert.equal(failures, 0);
  assert.ok(published.patch.length < 1000);
  assert.deepEqual(
    inlineComments(published.report, published.patch).map(
      (comment) => comment.path,
    ),
    ["a-large.txt", "z-later.txt"],
  );
});

test("worker uses the service-saved completed report if its local report is absent", async (t) => {
  const f = await local(t);
  let next = { job: { ...f.job, report, session } },
    published;
  const worker = (f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action, body) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "report") published = body;
        return { ok: true };
      },
      review: async () =>
        assert.fail("completed report must not invoke provider"),
    }),
  ));
  await until(() => published);
  assert.equal(published.report.summary, report.summary);
});

test("drain includes work returned by an already pending claim", async (t) => {
  const f = await local(t),
    claim = deferred(),
    began = deferred(),
    finish = deferred();
  f.beforeClose = () => finish.resolve();
  let claims = 0,
    finished = false;
  const worker = (f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action) =>
        action === "next" ? (++claims, claim.promise) : { ok: true },
      review: async () => {
        began.resolve();
        await finish.promise;
        finished = true;
        return report;
      },
    }),
  ));
  let drained = false;
  const drain = worker.drain().then(() => {
    drained = true;
  });
  claim.resolve({ job: f.job });
  await began.promise;
  assert.equal(drained, false);
  finish.resolve();
  await drain;
  assert.equal(finished, true);
  assert.equal(claims, 1);
});

test("close waits for provider cancellation cleanup and keeps the recorded session", async (t) => {
  const f = await local(t),
    began = deferred(),
    cleaning = deferred(),
    release = deferred();
  let next = { job: f.job },
    recorded,
    published = false;
  const worker = (f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action, body) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "session") recorded = body.session;
        if (action === "report") published = true;
        return { ok: true };
      },
      review: async ({ signal, onSession }) => {
        await onSession(session);
        began.resolve();
        try {
          await sleep(10000, signal);
        } finally {
          cleaning.resolve();
          await release.promise;
        }
      },
    }),
  ));
  await began.promise;
  let stopped = false;
  const stopping = worker.close().then(() => {
    stopped = true;
  });
  await cleaning.promise;
  assert.equal(stopped, false);
  release.resolve();
  await stopping;
  assert.equal(recorded, session);
  assert.equal(published, false);
});

test("worker logs redact arbitrary configured tokens and encoded Git authentication", async (t) => {
  const f = await local(t),
    token = "installation-secret-without-standard-prefix",
    errors = [];
  let next = { job: f.job, token },
    failed = false;
  const worker = (f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      logger: { error: (...args) => errors.push(args.join(" ")) },
      request: async (_, action) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "failed") failed = true;
        return { ok: true };
      },
      review: async () => {
        throw new Error(
          [
            token,
            f.config.worker.token,
            f.config.adminToken,
            Buffer.from(`x-access-token:${token}`).toString("base64"),
          ].join(" "),
        );
      },
    }),
  ));
  await until(() => failed);
  const logs = `${await readFile(join(f.root, "logs/job1.log"), "utf8")} ${errors.join(" ")}`;
  for (const secret of [
    token,
    f.config.worker.token,
    f.config.adminToken,
    Buffer.from(`x-access-token:${token}`).toString("base64"),
  ])
    assert.equal(logs.includes(secret), false);
});

async function bareFixture(root) {
  const working = join(root, "working"),
    bare = join(root, "fixture.git");
  await mkdir(working);
  const git = (args) => processRun("git", ["-C", working, ...args]);
  await git(["init", "-b", "main"]);
  await writeFile(
    join(working, "AGENTS.md"),
    "Review meaningful correctness issues.\n",
  );
  await writeFile(join(working, "code.js"), "export const answer = 1;\n");
  await git(["add", "."]);
  const commit = async (message) =>
    git([
      "-c",
      "user.name=Crow Test",
      "-c",
      "user.email=crow@example.test",
      "commit",
      "-m",
      message,
    ]);
  await commit("base");
  const base = (await git(["rev-parse", "HEAD"])).stdout.trim();
  await writeFile(join(working, "code.js"), "export const answer = 2;\n");
  await git(["add", "."]);
  await commit("first change");
  const head = (await git(["rev-parse", "HEAD"])).stdout.trim();
  await writeFile(join(working, "code.js"), "export const answer = 3;\n");
  await git(["add", "."]);
  await commit("second change");
  const newer = (await git(["rev-parse", "HEAD"])).stdout.trim();
  await git(["checkout", "-b", "updated-target", base]);
  await writeFile(
    join(working, "AGENTS.md"),
    "New target rules introduced after the review began.\n",
  );
  await git(["add", "."]);
  await commit("change target guidance");
  const updatedTarget = (await git(["rev-parse", "HEAD"])).stdout.trim();
  await git(["clone", "--bare", working, bare]);
  return {
    dir: bare,
    base,
    head,
    newer,
    updatedTarget,
    target: "main",
    targetSha: base,
  };
}
async function integrated(
  t,
  review,
  { beforeWorker, request = workerRequest } = {},
) {
  const f = await local(t);
  f.source = await bareFixture(f.root);
  f.config.port = 0;
  f.config.catchUp.enabled = false;
  f.config.app = {
    id: 1,
    pem: "test-private-key",
    slug: "crow-test",
    botId: 42,
    webhookSecret: "test-webhook-secret",
  };
  const state = {
    pr: {
      number: 1,
      state: "open",
      draft: false,
      user: { id: 1, login: "owner" },
      head: { sha: f.source.head },
      base: { ref: "main", sha: f.source.base },
    },
    reviews: [],
    published: [],
    failures: 0,
  };
  const github = {
    token: async () => "fake-installation-token",
    pr: async () => structuredClone(state.pr),
    audit: async () => {},
    reviews: async () => state.reviews,
    status: async () => ({ id: 123 }),
    publish: async (_repo, number, _token, body, head, comments) => {
      state.published.push({ number, body, head, comments });
      if (state.failures > 0) {
        state.failures--;
        throw new Error("GitHub temporary failure");
      }
      const result = {
        id: 321,
        user: { id: 42 },
        body,
        html_url:
          "https://github.com/owner/project/pull/1#pullrequestreview-321",
      };
      state.reviews.push(result);
      return result;
    },
  };
  const service = (f.service = await startService(f.config, f.root, {
    github,
    logger: quiet,
  }));
  f.config.port = service.server.address().port;
  f.config.serviceUrl = `http://127.0.0.1:${f.config.port}`;
  service.store.enroll({
    name: "owner/project",
    installation: 1,
    worker: f.config.worker.id,
    policy: "selected",
    authors: ["owner"],
    requesters: ["owner"],
    excluded: [],
    settings: {},
  });
  await beforeWorker?.({ ...f, service, state });
  const worker = (f.worker = await startWorker(f.config, f.root, {
    prepare: async (_root, _job, pr) => ({
      ...f.source,
      head: pr.head.sha,
      targetSha: pr.base.sha,
    }),
    review,
    request,
    heartbeatMs: 20,
    pollMs: 10,
    reconnectMs: 10,
    logger: quiet,
  }));
  const event = async (delivery) => {
    const body = JSON.stringify({
      action: "synchronize",
      number: 1,
      repository: { full_name: "owner/project" },
    });
    const response = await fetch(`${f.config.serviceUrl}/webhooks/github`, {
      method: "POST",
      body,
      headers: {
        "x-github-event": "pull_request",
        "x-github-delivery": delivery,
        "x-hub-signature-256": `sha256=${createHmac("sha256", f.config.app.webhookSecret).update(body).digest("hex")}`,
      },
    });
    assert.equal(response.status, 202);
    return response.json();
  };
  return {
    ...f,
    service,
    worker,
    state,
    event,
    jobs: () => service.store.all("jobs"),
  };
}

test("signed event reaches a pinned advisory report; failed GitHub publication never repeats inference", async (t) => {
  let calls = 0;
  const f = await integrated(t, async ({ onSession, guidance, source }) => {
    calls++;
    await onSession(session);
    assert.equal(guidance.files[0].path, "AGENTS.md");
    assert.equal(source.base, f.source.base);
    return {
      summary: "Constant change alters the result.",
      findings: [
        {
          title: "Changed expected result",
          body: "Consumers expecting 1 now receive 2.",
          path: "code.js",
          line: 1,
          severity: "low",
        },
      ],
    };
  });
  f.state.failures = 1;
  assert.deepEqual(await f.event("event1"), { accepted: true });
  assert.deepEqual(await f.event("event1"), { accepted: false });
  await until(
    () => f.jobs().some((j) => j.state === "publishing" && j.publishAt),
    "failed publication",
  );
  const saved = f.jobs()[0];
  assert.ok(saved.report);
  assert.equal(saved.session, session);
  f.service.store.updateJob(saved.id, { publishAt: 0 });
  await until(() => f.jobs()[0].state === "completed", "publication retry");
  assert.equal(calls, 1);
  assert.equal(f.state.published.length, 2);
  const published = f.state.published[1];
  assert.equal(published.head, f.source.head);
  assert.equal(metadata(published.body).base, f.source.base);
  assert.equal(published.comments[0].line, 1);
});

test("service pause reaches the worker and resume keeps its provider session", async (t) => {
  let calls = 0,
    cancelled = false;
  const f = await integrated(t, async ({ job, signal, onSession }) => {
    calls++;
    if (calls === 1) {
      await onSession(session);
      try {
        await sleep(20000, signal);
      } finally {
        cancelled = true;
      }
    }
    assert.equal(job.session, session);
    return report;
  });
  await f.event("pause-event");
  await until(() => f.jobs()[0]?.session === session);
  await f.service.admin("pause", { repo: "owner/project", number: 1 });
  await until(() => cancelled, "provider cancellation");
  assert.equal(f.jobs()[0].state, "paused");
  assert.equal(f.state.published.length, 0);
  await f.service.admin("resume", { repo: "owner/project", number: 1 });
  await until(() => f.jobs()[0].state === "completed", "resumed publication");
  assert.equal(calls, 2);
  assert.equal(f.jobs()[0].session, session);
});

test("new PR head cancels older work and only publishes the current comparison", async (t) => {
  let olderCancelled = false;
  const f = await integrated(t, async ({ source, signal, onSession }) => {
    await onSession(session);
    if (source.head === f.source.head) {
      try {
        await sleep(20000, signal);
      } finally {
        olderCancelled = true;
      }
    }
    return report;
  });
  await f.event("old-head");
  await until(() => f.jobs()[0]?.session);
  f.state.pr.head.sha = f.source.newer;
  await f.event("new-head");
  await until(
    () => f.jobs().some((j) => j.state === "completed"),
    "new head review",
  );
  assert.equal(olderCancelled, true);
  assert.equal(f.jobs()[0].state, "superseded");
  assert.equal(f.state.published.length, 1);
  assert.equal(f.state.published[0].head, f.source.newer);
});

test("worker reconciles valid saved session IDs and skips linked or invalid local files", async (t) => {
  const f = await local(t);
  await atomic(join(f.root, "reviews/good-job/session.json"), { id: session });
  await atomic(join(f.root, "reviews/invalid-job/session.json"), {
    id: "not-a-session",
  });
  await atomic(join(f.root, "outside/session.json"), { id: session });
  await symlink(join(f.root, "outside"), join(f.root, "reviews/linked-job"));
  await mkdir(join(f.root, "reviews/linked-file"));
  await symlink(
    join(f.root, "outside/session.json"),
    join(f.root, "reviews/linked-file/session.json"),
  );
  let advertised;
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action, body) => {
        if (action === "next") {
          advertised = body.sessions;
          return null;
        }
        return { ok: true };
      },
    }),
  );
  await until(() => advertised);
  assert.deepEqual(advertised, { "good-job": session });
});

test("a failed session RPC is reconciled from memory before another review attempt", async (t) => {
  const f = await local(t);
  let next = { job: f.job },
    failed = false,
    completed = false,
    calls = 0;
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action, body) => {
        if (action === "next") {
          if (failed) {
            assert.equal(body.sessions[f.job.id], session);
            failed = false;
            return {
              job: {
                ...f.job,
                lease: "second-lease",
                session: body.sessions[f.job.id],
              },
            };
          }
          const value = next;
          next = null;
          return value;
        }
        if (action === "session")
          throw new Error("Lost connection before session acknowledgment");
        if (action === "failed") failed = true;
        if (action === "report") completed = true;
        return { ok: true };
      },
      review: async ({ job, onSession }) => {
        if (++calls === 1) await onSession(session);
        assert.equal(job.session, session);
        return report;
      },
    }),
  );
  await until(() => completed);
  assert.equal(calls, 2);
});

test("worker status records connection, active work, draining and stopped without credentials", async (t) => {
  const f = await local(t),
    began = deferred(),
    release = deferred();
  f.beforeClose = () => release.resolve();
  let next = { job: f.job };
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        return { ok: true };
      },
      review: async () => {
        began.resolve();
        await release.promise;
        return report;
      },
    }),
  );
  await began.promise;
  const file = join(f.root, "worker-status.json");
  const running = await until(async () => {
    const value = await json(file, null);
    return value?.active.length && value;
  });
  assert.equal(running.state, "running");
  assert.equal(running.connection, "connected");
  assert.deepEqual(running.active, [
    { id: f.job.id, repo: f.job.repo, number: 1, state: "reviewing" },
  ]);
  const draining = f.worker.drain();
  await until(async () => (await json(file, null))?.state === "draining");
  release.resolve();
  await draining;
  await f.worker.close();
  const stopped = await json(file, null);
  assert.equal(stopped.state, "stopped");
  assert.equal(stopped.connection, "stopped");
  assert.deepEqual(stopped.active, []);
  assert.equal((await stat(file)).mode & 0o777, 0o600);
  const text = JSON.stringify(stopped);
  assert.equal(text.includes(f.config.worker.token), false);
  assert.equal(text.includes(f.config.adminToken), false);
});

test("service recovers an unacknowledged session from the returning worker after a restart", async (t) => {
  let calls = 0;
  const f = await integrated(
    t,
    async ({ job }) => {
      calls++;
      assert.equal(job.session, session);
      return report;
    },
    {
      beforeWorker: async ({ root, service, state, source }) => {
        const repo = service.store.get("repos", "owner/project");
        const job = service.store.queue(repo, state.pr);
        service.store.updateJob(job.id, {
          state: "paused",
          autoRecover: true,
          session: null,
          comparison: source,
          startedAt: Date.now(),
        });
        await atomic(join(root, "reviews", job.id, "session.json"), {
          id: session,
          comparison: source,
        });
      },
    },
  );
  await until(
    () => f.jobs()[0]?.state === "completed",
    "session acknowledgment reconciliation",
  );
  assert.equal(calls, 1);
  assert.equal(f.jobs()[0].session, session);
});

test("lost session acknowledgment followed by successful failure RPC retains automatic recovery", async (t) => {
  let calls = 0,
    lost = false;
  const f = await integrated(
    t,
    async ({ job, onSession }) => {
      calls++;
      if (calls === 1) await onSession(session);
      assert.equal(job.session, session);
      return report;
    },
    {
      beforeWorker: async ({ config }) => {
        config.worker.retry.delayMs = 1;
      },
      request: async (config, action, body, signal) => {
        if (action === "session" && !lost) {
          lost = true;
          throw new Error("Session acknowledgment connection lost");
        }
        if (action === "failed") assert.equal(body.session, session);
        return workerRequest(config, action, body, signal);
      },
    },
  );
  await f.event("missing-session-ack");
  await until(
    () => f.jobs()[0]?.state === "completed",
    "retry after lost session acknowledgment",
  );
  assert.equal(calls, 2);
  assert.equal(f.jobs()[0].session, session);
});

test("immediate pause then resume waits for old provider cancellation before reclaiming the job", async (t) => {
  let calls = 0,
    live = 0,
    maxLive = 0;
  const f = await integrated(
    t,
    async ({ signal, onSession }) => {
      calls++;
      live++;
      maxLive = Math.max(maxLive, live);
      try {
        await onSession(session);
        if (calls === 1) {
          try {
            await sleep(20000, signal);
          } finally {
            await sleep(40);
          }
        }
        return report;
      } finally {
        live--;
      }
    },
    {
      beforeWorker: async ({ config }) => {
        config.worker.concurrency = 3;
      },
    },
  );
  await f.event("pause-resume-race");
  await until(() => f.jobs()[0]?.session);
  await f.service.admin("pause", { repo: "owner/project", number: 1 });
  await f.service.admin("resume", { repo: "owner/project", number: 1 });
  await until(() => f.jobs()[0]?.state === "completed", "resume after cleanup");
  assert.equal(calls, 2);
  assert.equal(maxLive, 1);
});

test("paused review retains original guidance when target instructions change before resume", async (t) => {
  let calls = 0,
    cancelled = false;
  const seen = [];
  const f = await integrated(t, async ({ signal, guidance, onSession }) => {
    seen.push(guidance);
    calls++;
    if (calls === 1) {
      await onSession(session);
      try {
        await sleep(20000, signal);
      } finally {
        cancelled = true;
      }
    }
    return report;
  });
  await f.event("pinned-guidance");
  await until(() => f.jobs()[0]?.session);
  await f.service.admin("pause", { repo: "owner/project", number: 1 });
  await until(() => cancelled);
  f.state.pr.base.sha = f.source.updatedTarget;
  await f.service.admin("resume", { repo: "owner/project", number: 1 });
  await until(() => f.jobs()[0]?.state === "completed");
  assert.equal(calls, 2);
  assert.deepEqual(seen[0], seen[1]);
  assert.match(seen[1].files[0].body, /Review meaningful correctness issues/);
  assert.equal(f.jobs()[0].comparison.targetSha, f.source.updatedTarget);
  assert.equal(f.jobs()[0].guidanceTargetSha, f.source.base);
  await f.service.admin("review", { repo: "owner/project", number: 1 });
  await until(
    () => f.jobs()[1]?.state === "completed",
    "new manual review with current guidance",
  );
  assert.equal(calls, 3);
  assert.match(seen[2].files[0].body, /New target rules introduced/);
  assert.notEqual(seen[2].fingerprint, seen[0].fingerprint);
});

test("worker reconstructs missing guidance from the original target revision only", async (t) => {
  const f = await local(t),
    files = [{ path: "AGENTS.md", body: "Original review instructions" }];
  let next = {
      job: {
        ...f.job,
        guidanceFingerprint: hash(files),
        guidanceTargetSha: f.source.targetSha,
        comparison: f.source,
        session,
      },
    },
    sent;
  const current = { ...f.source, targetSha: "c".repeat(40) };
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      prepare: async () => current,
      readGuidance: async (source) => {
        assert.equal(source.targetSha, f.source.targetSha);
        return { files, fingerprint: hash(files) };
      },
      request: async (_, action, body) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "report") sent = body;
        return { ok: true };
      },
      review: async ({ guidance, source }) => {
        assert.deepEqual(guidance.files, files);
        assert.equal(source.targetSha, current.targetSha);
        return report;
      },
    }),
  );
  await until(() => sent);
  const restored = await json(join(f.root, "reviews/job1/guidance.json"));
  assert.equal(restored.targetSha, f.source.targetSha);
});

test("worker refuses to resume with altered cached guidance", async (t) => {
  const f = await local(t),
    original = [{ path: "AGENTS.md", body: "Original rules" }];
  await atomic(join(f.root, "reviews/job1/guidance.json"), {
    files: [{ path: "AGENTS.md", body: "Changed rules" }],
    fingerprint: hash(original),
    targetSha: f.source.targetSha,
  });
  let next = {
      job: { ...f.job, guidanceFingerprint: hash(original), session },
    },
    failed;
  f.worker = await startWorker(
    f.config,
    f.root,
    workerOptions(f, {
      request: async (_, action, body) => {
        if (action === "next") {
          const value = next;
          next = null;
          return value;
        }
        if (action === "failed") failed = body;
        return { ok: true };
      },
      review: async () => assert.fail("changed rules must not reach provider"),
    }),
  );
  await until(() => failed);
  assert.equal(failed.kind, "restart");
});
