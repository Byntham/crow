import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHmac } from "node:crypto";
import { startService } from "../dist/lib/service.mjs";
import { defaults } from "../dist/lib/config.mjs";
import { Store } from "../dist/lib/store.mjs";

const head = "a".repeat(40),
  base = "b".repeat(40),
  targetSha = "c".repeat(40);
const pr = (number = 1, extra = {}) => ({
  number,
  state: "open",
  draft: false,
  user: { login: "alice" },
  head: { sha: head },
  base: { ref: "main", sha: targetSha },
  ...extra,
});
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-service-")),
    config = defaults(root);
  Object.assign(config, {
    port: 0,
    operator: "alice",
    app: {
      id: 1,
      slug: "crow-test",
      pem: "private-app-key",
      webhookSecret: "webhook-secret",
      botId: 42,
    },
  });
  config.catchUp.enabled = false;
  config.worker.model = "provider-model";
  config.worker.effort = "high";
  const store = new Store(join(root, "service.sqlite")),
    prs = new Map([[1, pr()]]),
    published = [],
    statuses = [],
    errors = [];
  const github = {
    token: async () => "private-installation-token",
    pr: async (_r, n) => structuredClone(prs.get(n)),
    prs: async () => [...prs.values()].map((x) => structuredClone(x)),
    reviews: async () => [],
    publish: async (...args) => {
      published.push(args);
      return {
        id: published.length,
        html_url: "https://github.com/owner/project/pull/1#pullrequestreview-1",
      };
    },
    status: async (...args) => {
      statuses.push(args);
      return { id: 1 };
    },
    audit: async () => {},
    installation: async () => ({ id: 11 }),
    request: async (path) =>
      path === "/user" ? { login: "alice" } : { permissions: { admin: true } },
  };
  store.enroll({
    name: "owner/project",
    installation: 11,
    worker: config.worker.id,
    policy: "selected",
    authors: ["alice"],
    requesters: ["alice"],
    excluded: [],
    settings: {},
  });
  const svc = await startService(config, root, {
    github,
    store,
    logger: { error: (...x) => errors.push(x.join(" ")) },
  });
  config.port = svc.server.address().port;
  const origin = `http://127.0.0.1:${config.port}`;
  t.after(async () => {
    await svc.close();
    await rm(root, { recursive: true, force: true });
  });
  const call = async (path, body = {}, token = config.adminToken) => {
    const res = await fetch(origin + path, {
      method: path === "/health" || path === "/admin/status" ? "GET" : "POST",
      headers: {
        "Content-Type": "application/json",
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
      },
      body:
        path === "/health" || path === "/admin/status"
          ? undefined
          : JSON.stringify(body),
    });
    return { status: res.status, data: await res.json() };
  };
  const worker = (action, body = {}) =>
    call("/worker/" + action, body, config.worker.token);
  const webhook = async (
    type,
    payload,
    delivery = "delivery-1",
    valid = true,
  ) => {
    const body = JSON.stringify(payload),
      sig = `sha256=${createHmac("sha256", config.app.webhookSecret).update(body).digest("hex")}`;
    const res = await fetch(origin + "/webhooks/github", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-github-event": type,
        "x-github-delivery": delivery,
        "x-hub-signature-256": valid ? sig : "sha256=invalid",
      },
      body,
    });
    return { status: res.status, data: await res.json() };
  };
  return {
    config,
    store,
    svc,
    prs,
    published,
    statuses,
    errors,
    call,
    worker,
    webhook,
    github,
  };
}
async function until(fn) {
  const end = Date.now() + 3500;
  while (Date.now() < end) {
    const value = fn();
    if (value) return value;
    await new Promise((r) => setTimeout(r, 25));
  }
  assert.fail("Timed out waiting for service state");
}
const event = (number = 1, action = "opened") => ({
  action,
  number,
  repository: { full_name: "owner/project" },
  pull_request: { body: "PRIVATE PR CONTENT THAT MUST NOT BE RETAINED" },
});
async function claim(f) {
  await f.call("/admin/review", { repo: "owner/project", number: 1 });
  const r = await f.worker("next");
  assert.equal(r.status, 200);
  assert.ok(r.data?.job);
  return r.data.job;
}
const comparison = { head, base, target: "main", targetSha };

test("service exposes minimal health and requires separate administrative and worker credentials", async (t) => {
  const f = await fixture(t);
  assert.deepEqual((await f.call("/health", {}, null)).data, {
    ok: true,
    service: "crow",
    configured: true,
  });
  assert.equal((await f.call("/admin/status", {}, null)).status, 401);
  assert.equal(
    (await f.call("/admin/pair", {}, f.config.worker.token)).status,
    401,
  );
  assert.equal(
    (await f.call("/worker/next", {}, f.config.adminToken)).status,
    401,
  );
  const text = JSON.stringify((await f.call("/health", {}, null)).data);
  for (const secret of [
    "private-app-key",
    "webhook-secret",
    f.config.worker.token,
    f.config.adminToken,
  ])
    assert.ok(!text.includes(secret));
});
test("signed webhook acknowledgement persists minimal event and deduplicates delivery", async (t) => {
  const f = await fixture(t);
  assert.equal(
    (await f.webhook("pull_request", event(), "delivery", false)).status,
    401,
  );
  assert.deepEqual(f.store.events(), []);
  assert.deepEqual(await f.webhook("pull_request", event(), "delivery"), {
    status: 202,
    data: { accepted: true },
  });
  assert.equal(f.store.events()[0].number, 1);
  assert.ok(!JSON.stringify(f.store.events()).includes("PRIVATE PR CONTENT"));
  assert.deepEqual(await f.webhook("pull_request", event(), "delivery"), {
    status: 202,
    data: { accepted: false },
  });
  await until(() => f.store.all("jobs").length === 1);
  assert.deepEqual(f.store.events(), []);
});
test("webhooks cancel newly drafted PRs and reject unauthorized authors", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  f.prs.set(1, pr(1, { draft: true }));
  await f.webhook("pull_request", event(1, "converted_to_draft"));
  await until(() => f.store.get("jobs", job.id).state === "cancelled");
  f.prs.set(2, pr(2, { user: { login: "mallory" } }));
  await f.webhook("pull_request", event(2), "delivery-2");
  await until(() => f.store.events().length === 0);
  assert.equal(
    f.store.all("jobs").some((j) => j.number === 2),
    false,
  );
});
test("synchronizing a PR supersedes existing lease and schedules latest revision", async (t) => {
  const f = await fixture(t);
  const old = await claim(f);
  f.prs.set(1, pr(1, { head: { sha: "d".repeat(40) } }));
  await f.webhook("pull_request", event(1, "synchronize"));
  await until(() => f.store.get("jobs", old.id).state === "superseded");
  assert.deepEqual(
    (await f.worker("heartbeat", { id: old.id, lease: old.lease })).data,
    { cancel: true },
  );
  const next = await f.worker("next");
  assert.equal(next.data.job.head, "d".repeat(40));
  assert.notEqual(next.data.job.id, old.id);
});
test("worker cannot access another worker job or write using a stale lease", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  const pair = await f.call("/admin/pair", {
    id: "other",
    token: "other-worker-token-with-at-least-32-characters",
  });
  assert.equal(pair.status, 200);
  assert.deepEqual(
    (
      await f.call(
        "/worker/report",
        {
          id: job.id,
          lease: job.lease,
          report: { summary: "fake", findings: [] },
        },
        "other-worker-token-with-at-least-32-characters",
      )
    ).data,
    { cancel: true },
  );
  assert.deepEqual(
    (await f.worker("heartbeat", { id: job.id, lease: "invalid" })).data,
    { cancel: true },
  );
  assert.equal(f.store.get("jobs", job.id).state, "reviewing");
});
test("invalid final output remains unpublished and valid report is durable before publication", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  const identity = { id: job.id, lease: job.lease };
  assert.equal(
    (
      await f.worker("comparison", {
        ...identity,
        comparison,
        guidanceFingerprint: "trusted-guidance",
      })
    ).status,
    200,
  );
  assert.equal(
    (
      await f.worker("report", {
        ...identity,
        report: { summary: "", findings: [] },
      })
    ).status,
    400,
  );
  assert.equal(f.store.get("jobs", job.id).state, "reviewing");
  assert.equal(f.published.length, 0);
  assert.equal(
    (
      await f.worker("report", {
        ...identity,
        report: { summary: "No problems found.", findings: [] },
        patch: "",
      })
    ).status,
    200,
  );
  assert.equal(
    f.store.get("jobs", job.id).report.summary,
    "No problems found.",
  );
  await until(() => f.store.get("jobs", job.id).state === "completed");
  assert.equal(f.published.length, 1);
  assert.equal(f.published[0][4], head);
});
test("provider errors resume saved sessions and respect configured retry exhaustion", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  assert.equal(
    (
      await f.worker("session", {
        ...identity,
        session: "12345678-1234-1234-1234-123456789abc",
      })
    ).status,
    200,
  );
  await f.worker("failed", {
    ...identity,
    kind: "transient",
    retryAfter: 60000,
  });
  let saved = f.store.get("jobs", job.id);
  assert.equal(saved.state, "retrying");
  assert.equal(saved.retries, 1);
  assert.ok(saved.nextAt >= Date.now() + 59000);
  assert.equal((await f.worker("next")).data, null);
  f.store.updateJob(job.id, { retries: 10, state: "reviewing" });
  await f.worker("failed", { ...identity, kind: "transient" });
  saved = f.store.get("jobs", job.id);
  assert.equal(saved.state, "paused");
  assert.equal(saved.session, "12345678-1234-1234-1234-123456789abc");
  assert.equal(f.published.length, 0);
});
test("authentication and quota failures preserve work without scheduling transient retries", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  await f.worker("session", {
    ...identity,
    session: "12345678-1234-1234-1234-123456789abc",
  });
  await f.worker("failed", { ...identity, kind: "auth" });
  assert.equal(f.store.get("jobs", job.id).state, "paused");
  assert.match(f.store.get("jobs", job.id).reason, /login/);
  assert.equal(f.store.get("jobs", job.id).nextAt, 0);
});
test("new enrollment excludes initial backlog and checks GitHub repository authority", async (t) => {
  const f = await fixture(t);
  f.store.delete("repos", "owner/project");
  f.github.request = async (path) =>
    path === "/user" ? { login: "alice" } : { permissions: { push: true } };
  assert.equal(
    (
      await f.call("/admin/enroll", {
        repo: "owner/project",
        githubToken: "operator-token",
      })
    ).status,
    400,
  );
  assert.equal(f.store.get("repos", "owner/project"), null);
  f.github.request = async (path) =>
    path === "/user" ? { login: "alice" } : { permissions: { maintain: true } };
  assert.equal(
    (
      await f.call("/admin/enroll", {
        repo: "owner/project",
        githubToken: "operator-token",
      })
    ).status,
    200,
  );
  assert.deepEqual(f.store.get("repos", "owner/project").excluded, [1]);
  assert.deepEqual(f.store.all("jobs"), []);
  await f.call("/admin/catch-up", { repo: "owner/project" });
  assert.deepEqual(f.store.all("jobs"), []);
  await f.webhook("pull_request", event(1, "synchronize"));
  await until(() => f.store.all("jobs").length === 1);
  assert.deepEqual(f.store.get("repos", "owner/project").excluded, []);
});
test("large catch-up is held across repeated scans and released explicitly", async (t) => {
  const f = await fixture(t);
  f.config.catchUp.threshold = 1;
  f.prs.set(2, pr(2));
  let r = await f.call("/admin/catch-up", { repo: "owner/project" });
  assert.deepEqual(r.data, { "owner/project": { queued: 0, held: 2 } });
  assert.equal((await f.worker("next")).data, null);
  await f.call("/admin/catch-up", { repo: "owner/project" });
  assert.ok(f.store.all("jobs").every((j) => j.state === "held"));
  assert.deepEqual(
    (await f.call("/admin/release", { repo: "owner/project" })).data,
    { released: 2 },
  );
  assert.ok((await f.worker("next")).data.job);
});
test("ordinary comments and unauthorized explicit requests never queue model work", async (t) => {
  const f = await fixture(t);
  for (const [i, body, actor] of [
    [1, "Please explain this", "alice"],
    [2, "@crow review", "mallory"],
  ])
    await f.webhook(
      "issue_comment",
      {
        action: "created",
        repository: { full_name: "owner/project" },
        issue: { number: 1, pull_request: {} },
        comment: { body, user: { login: actor } },
      },
      `comment-${i}`,
    );
  await until(() => f.store.events().length === 0);
  assert.deepEqual(f.store.all("jobs"), []);
  await f.webhook(
    "issue_comment",
    {
      action: "created",
      repository: { full_name: "owner/project" },
      issue: { number: 1, pull_request: {} },
      comment: { body: "@crow review", user: { login: "alice" } },
    },
    "comment-3",
  );
  await until(() => f.store.all("jobs").length === 1);
});

test("reports cannot become publishable before a validated comparison is recorded", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  assert.equal(
    (
      await f.worker("comparison", {
        ...identity,
        comparison: { ...comparison, base: `invalid-${base}-suffix` },
      })
    ).status,
    400,
  );
  assert.equal(
    (
      await f.worker("comparison", {
        ...identity,
        comparison: { ...comparison, targetSha: undefined },
      })
    ).status,
    400,
  );
  assert.equal(
    (
      await f.worker("report", {
        ...identity,
        report: { summary: "No problems found.", findings: [] },
      })
    ).status,
    400,
  );
  assert.equal(f.store.get("jobs", job.id).state, "reviewing");
  assert.equal(f.published.length, 0);
});
test("failed GitHub publication retries the durable report without another worker claim", async (t) => {
  const f = await fixture(t);
  const original = f.github.publish;
  let attempts = 0;
  f.github.publish = async (...args) => {
    if (++attempts === 1) throw new Error("temporary GitHub error");
    return original(...args);
  };
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  await f.worker("report", {
    ...identity,
    report: { summary: "Completed analysis.", findings: [] },
  });
  await until(() => f.store.get("jobs", job.id).publishAt);
  assert.equal(f.store.get("jobs", job.id).state, "publishing");
  assert.equal((await f.worker("next")).data, null);
  f.store.updateJob(job.id, { publishAt: 0 });
  await until(() => f.store.get("jobs", job.id).state === "completed");
  assert.equal(attempts, 2);
  assert.equal(f.published.length, 1);
});
test("other bots and deleted users cannot forge this installation completion marker", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  const { marker } = await import("../dist/lib/report.mjs");
  f.github.reviews = async () => [
    {
      user: null,
      body: marker({ job: job.id, ...comparison }),
      html_url: "https://github.com/owner/project/pull/1#forged-deleted",
    },
    {
      user: { id: 999 },
      body: marker({ job: job.id }),
      html_url: "https://github.com/owner/project/pull/1#forged",
    },
  ];
  await f.worker("comparison", { ...identity, comparison });
  await f.worker("report", {
    ...identity,
    report: { summary: "Completed analysis.", findings: [] },
  });
  await until(() => f.store.get("jobs", job.id).state === "completed");
  assert.equal(f.published.length, 1);
  assert.ok(!f.store.get("jobs", job.id).reviewUrl.includes("forged"));
});

test("catch-up rechecks a completed head when target tip changes without repeating inference for same merge base", async (t) => {
  const f = await fixture(t);
  const repo = f.store.get("repos", "owner/project");
  const original = f.store.queue(repo, pr());
  f.store.updateJob(original.id, {
    state: "completed",
    comparison,
    reviewUrl: "https://github.com/owner/project/pull/1#old",
  });
  f.store.put("completed", `${original.key}:${head}:main:${base}`, {
    comparison,
    reviewUrl: "https://github.com/owner/project/pull/1#old",
  });
  f.prs.set(1, pr(1, { base: { ref: "main", sha: "d".repeat(40) } }));
  const result = await f.call("/admin/catch-up", { repo: repo.name });
  assert.equal(result.data[repo.name].queued, 1);
  const next = (await f.worker("next")).data.job;
  assert.notEqual(next.id, original.id);
  const decision = await f.worker("comparison", {
    id: next.id,
    lease: next.lease,
    comparison: { ...comparison, targetSha: "d".repeat(40) },
  });
  assert.deepEqual(decision.data, { skip: true });
  assert.equal(f.store.get("jobs", next.id).state, "completed");
  assert.equal(f.published.length, 0);
});
test("completion can be reconstructed from authenticated App markers but never another bot", async (t) => {
  const f = await fixture(t);
  const { marker } = await import("../dist/lib/report.mjs");
  const repo = f.store.get("repos", "owner/project");
  const queued = f.store.queue(repo, pr());
  f.github.reviews = async () => [
    {
      user: { id: 999 },
      body: marker(comparison),
      html_url: "https://github.com/owner/project/pull/1#forged",
    },
    {
      user: { id: 42 },
      body: marker(comparison),
      html_url: "https://github.com/owner/project/pull/1#own-review",
    },
  ];
  const job = (await f.worker("next")).data.job;
  assert.equal(job.id, queued.id);
  assert.deepEqual(
    (await f.worker("comparison", { id: job.id, lease: job.lease, comparison }))
      .data,
    { skip: true },
  );
  assert.equal(
    f.store.get("jobs", job.id).reviewUrl,
    "https://github.com/owner/project/pull/1#own-review",
  );
  assert.equal(f.published.length, 0);
});
test("resuming with a changed merge base supersedes saved analysis and coalesces a duplicate event", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  await f.worker("session", {
    ...identity,
    session: "12345678-1234-1234-1234-123456789abc",
  });
  await f.call("/admin/pause", { repo: "owner/project", number: 1 });
  await f.call("/admin/resume", { repo: "owner/project", number: 1 });
  const resumed = (await f.worker("next")).data.job;
  assert.equal(resumed.id, job.id);
  assert.deepEqual(
    (
      await f.worker("comparison", {
        id: resumed.id,
        lease: resumed.lease,
        comparison: { ...comparison, base: "e".repeat(40) },
      })
    ).data,
    { cancel: true },
  );
  assert.equal(f.store.get("jobs", job.id).state, "superseded");
  const replacement = f.store.all("jobs").find((j) => j.state === "queued");
  assert.ok(replacement);
  assert.notEqual(replacement.id, job.id);
  await f.webhook("pull_request", event(1, "synchronize"));
  await until(() => f.store.events().length === 0);
  assert.equal(
    f.store.all("jobs").filter((j) => j.state === "queued").length,
    1,
  );
});
test("saved report automatically verifies its comparison after an unrelated target advance", async (t) => {
  const f = await fixture(t);
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  f.prs.set(1, pr(1, { base: { ref: "main", sha: "d".repeat(40) } }));
  await f.worker("report", {
    ...identity,
    report: { summary: "Saved complete report.", findings: [] },
  });
  await until(() => f.store.get("jobs", job.id).state === "queued");
  assert.equal(f.published.length, 0);
  const resumed = (await f.worker("next")).data.job;
  assert.equal(resumed.id, job.id);
  assert.equal(resumed.report.summary, "Saved complete report.");
  const currentIdentity = { id: resumed.id, lease: resumed.lease };
  await f.worker("comparison", {
    ...currentIdentity,
    comparison: { ...comparison, targetSha: "d".repeat(40) },
  });
  await f.worker("report", { ...currentIdentity, report: resumed.report });
  await until(() => f.store.get("jobs", job.id).state === "completed");
  assert.equal(f.published.length, 1);
});

test("a failed stale claim cannot resurrect a job superseded during GitHub refresh", async (t) => {
  const f = await fixture(t);
  await f.call("/admin/review", { repo: "owner/project", number: 1 });
  const old = f.store.all("jobs")[0],
    original = f.github.pr;
  let failRefresh,
    entered = false;
  f.github.pr = async (...args) => {
    if (!entered) {
      entered = true;
      return new Promise((_resolve, reject) => {
        failRefresh = reject;
      });
    }
    return original(...args);
  };
  const claiming = f.worker("next");
  await until(() => entered);
  f.prs.set(1, pr(1, { head: { sha: "d".repeat(40) } }));
  await f.webhook("pull_request", event(1, "synchronize"));
  await until(() => f.store.get("jobs", old.id).state === "superseded");
  failRefresh(new Error("GitHub lookup interrupted"));
  assert.equal((await claiming).status, 400);
  assert.equal(f.store.get("jobs", old.id).state, "superseded");
  assert.equal(
    f.store.all("jobs").filter((j) => j.state === "queued").length,
    1,
  );
});
test("publication finishing after a push records historical review without reviving superseded job", async (t) => {
  const f = await fixture(t);
  let resolvePublication,
    entered = false;
  f.github.publish = async () => {
    entered = true;
    return new Promise((resolve) => {
      resolvePublication = resolve;
    });
  };
  const job = await claim(f),
    identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  await f.worker("report", {
    ...identity,
    report: { summary: "Historical complete report.", findings: [] },
  });
  await until(() => entered);
  f.prs.set(1, pr(1, { head: { sha: "d".repeat(40) } }));
  await f.webhook("pull_request", event(1, "synchronize"));
  await until(() => f.store.get("jobs", job.id).state === "superseded");
  resolvePublication({
    id: 3,
    html_url: "https://github.com/owner/project/pull/1#historical-review",
  });
  await until(() =>
    f.store.get("completed", `${job.key}:${head}:main:${base}`),
  );
  assert.equal(f.store.get("jobs", job.id).state, "superseded");
  assert.equal(
    f.store.all("jobs").filter((j) => j.state === "queued").length,
    1,
  );
});

test("resume cannot rewrite model metadata while a review is actively running", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  const result = await f.call("/admin/resume", {
    repo: "owner/project",
    number: 1,
    model: "another-model",
  });
  assert.equal(result.status, 400);
  assert.equal(f.store.get("jobs", job.id).settings.model, "provider-model");
});
test("a model override on a held job saves complete settings needed for retry recovery", async (t) => {
  const f = await fixture(t);
  const job = f.store.queue(f.store.get("repos", "owner/project"), pr(), {
    held: true,
  });
  assert.equal(
    (
      await f.call("/admin/resume", {
        repo: "owner/project",
        number: 1,
        model: "another-model",
      })
    ).status,
    200,
  );
  const next = (await f.worker("next")).data.job;
  assert.equal(next.id, job.id);
  assert.equal(next.settings.model, "another-model");
  assert.deepEqual(next.settings.retry, {
    mode: "fixed",
    count: 10,
    delayMs: 5000,
  });
  await f.worker("session", {
    id: next.id,
    lease: next.lease,
    session: "12345678-1234-1234-1234-123456789abc",
  });
  assert.equal(
    (
      await f.worker("failed", {
        id: next.id,
        lease: next.lease,
        kind: "transient",
      })
    ).status,
    200,
  );
  assert.equal(f.store.get("jobs", job.id).state, "retrying");
});
test("a paused-and-resumed review cannot be claimed until its previous process stops", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  await f.call("/admin/pause", { repo: "owner/project", number: 1 });
  f.store.updateJob(job.id, {
    session: "12345678-1234-1234-1234-123456789abc",
  });
  await f.call("/admin/resume", { repo: "owner/project", number: 1 });
  assert.equal((await f.worker("next", { active: [job.id] })).data, null);
  assert.equal(f.store.get("jobs", job.id).state, "queued");
  assert.equal((await f.worker("next", { active: [] })).data.job.id, job.id);
});

test("a replacement waits for obsolete work to stop while unrelated PRs can start", async (t) => {
  const f = await fixture(t);
  const old = await claim(f);
  f.prs.set(1, pr(1, { head: { sha: "d".repeat(40) } }));
  await f.call("/admin/restart", { repo: "owner/project", number: 1 });
  f.prs.set(2, pr(2));
  await f.call("/admin/review", { repo: "owner/project", number: 2 });
  const next = (await f.worker("next", { active: [old.id] })).data.job;
  assert.equal(next.number, 2);
  assert.equal(
    f.store.all("jobs").filter((j) => j.number === 1 && j.state === "queued")
      .length,
    1,
  );
});

test("author policy revocation cancels active reviews and requester settings stay separate", async (t) => {
  const f = await fixture(t);
  f.store.queue(f.store.get("repos", "owner/project"), pr());
  const work = (await f.worker("next")).data;
  assert.equal(
    (
      await f.call("/admin/config-repo", {
        repo: "owner/project",
        requesters: ["bob"],
      })
    ).status,
    200,
  );
  assert.equal(f.store.get("jobs", work.job.id).state, "reviewing");
  assert.equal(
    (
      await f.call("/admin/config-repo", {
        repo: "owner/project",
        authors: ["bob"],
      })
    ).status,
    200,
  );
  assert.equal(
    (await f.worker("heartbeat", { id: work.job.id, lease: work.job.lease }))
      .data.cancel,
    true,
  );
  assert.equal(
    (
      await f.call("/admin/config-repo", {
        repo: "owner/project",
        requesters: "everyone",
      })
    ).status,
    400,
  );
});

test("redelivered events from before enrollment do not import excluded initial backlog", async (t) => {
  const f = await fixture(t),
    repo = f.store.get("repos", "owner/project");
  repo.enrolledAt = Date.now();
  repo.excluded = [1];
  f.store.enroll(repo);
  const payload = event();
  payload.pull_request = {
    updated_at: new Date(repo.enrolledAt - 60000).toISOString(),
  };
  await f.webhook("pull_request", payload, "older-than-enrollment");
  await until(() => f.store.events().length === 0);
  assert.equal(f.store.all("jobs").length, 0);
});

test("concurrent provider failures cannot shorten the shared cooldown", async (t) => {
  const f = await fixture(t);
  const first = await claim(f);
  f.prs.set(2, pr(2));
  await f.call("/admin/review", { repo: "owner/project", number: 2 });
  const second = (await f.worker("next")).data.job;
  const session = "12345678-1234-1234-1234-123456789abc";
  await f.worker("failed", {
    id: first.id, lease: first.lease, session, kind: "transient", retryAfter: 60000,
  });
  const cooldown = f.store.get("state", "cooldown");
  assert.ok(cooldown >= Date.now() + 59000);
  await f.worker("failed", {
    id: second.id, lease: second.lease, session, kind: "transient",
  });
  assert.equal(f.store.get("state", "cooldown"), cooldown);
  assert.equal((await f.worker("next")).data, null);
});

test("target pushes revalidate saved work automatically and replace changed comparisons", async (t) => {
  const f = await fixture(t);
  const job = await claim(f), identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  await f.worker("session", {
    ...identity, session: "12345678-1234-1234-1234-123456789abc",
  });
  f.prs.set(1, pr(1, { base: { ref: "main", sha: "d".repeat(40) } }));
  await f.webhook("push", {
    repository: { full_name: "owner/project" }, ref: "refs/heads/main",
  });
  await until(() => f.store.events().length === 0);
  assert.equal(f.store.get("jobs", job.id).state, "reviewing");
  await f.worker("report", {
    ...identity, report: { summary: "Report for the old comparison.", findings: [] },
  });
  await until(() => f.store.get("jobs", job.id).state === "queued");
  const resumed = (await f.worker("next")).data.job;
  assert.equal(resumed.session, "12345678-1234-1234-1234-123456789abc");
  assert.ok(resumed.report);
  const result = await f.worker("comparison", {
    id: resumed.id, lease: resumed.lease,
    comparison: { ...comparison, base: "e".repeat(40), targetSha: "d".repeat(40) },
  });
  assert.deepEqual(result.data, { cancel: true });
  assert.equal(f.store.get("jobs", job.id).state, "superseded");
  const replacement = f.store.all("jobs").find((j) => j.state === "queued");
  assert.ok(replacement);
  assert.equal(replacement.session, null);
  assert.equal(replacement.report, null);
  assert.equal(f.published.length, 0);
});

test("target pushes and catch-up preserve an explicitly paused review", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  await f.worker("comparison", { id: job.id, lease: job.lease, comparison });
  await f.call("/admin/pause", { repo: "owner/project", number: 1 });
  f.prs.set(1, pr(1, { base: { ref: "main", sha: "d".repeat(40) } }));
  await f.webhook("push", {
    repository: { full_name: "owner/project" }, ref: "refs/heads/main",
  });
  await until(() => f.store.events().length === 0);
  await f.call("/admin/catch-up", { repo: "owner/project" });
  assert.equal(f.store.get("jobs", job.id).state, "paused");
  assert.equal((await f.worker("next")).data, null);
});

test("target revalidation does not undo a pause during publication refresh", async (t) => {
  const f = await fixture(t);
  const job = await claim(f), identity = { id: job.id, lease: job.lease };
  await f.worker("comparison", { ...identity, comparison });
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  let refreshing = false;
  f.github.pr = async () => {
    refreshing = true;
    await gate;
    return pr(1, { base: { ref: "main", sha: "d".repeat(40) } });
  };
  t.after(() => release());
  await f.worker("report", {
    ...identity, report: { summary: "Saved report.", findings: [] },
  });
  await until(() => refreshing);
  await f.call("/admin/pause", { repo: "owner/project", number: 1 });
  release();
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(f.store.get("jobs", job.id).state, "paused");
  assert.equal(f.published.length, 0);
});

for (const eligible of [true, false]) {
  test(`checkout head races refresh the PR and ${eligible ? "queue its current head" : "respect author authorization"}`, async (t) => {
    const f = await fixture(t);
    const job = await claim(f);
    f.prs.set(1, pr(1, {
      head: { sha: "d".repeat(40) }, user: { login: eligible ? "alice" : "mallory" },
    }));
    const result = await f.worker("failed", {
      id: job.id, lease: job.lease, kind: "superseded",
    });
    assert.deepEqual(result.data, { cancel: true });
    assert.equal(f.store.get("jobs", job.id).state, "superseded");
    const replacement = f.store.all("jobs").find((j) => j.state === "queued");
    assert.equal(!!replacement, eligible);
    if (eligible) assert.equal(replacement.head, "d".repeat(40));
  });
}

test("checkout recovery cannot undo an operator pause during GitHub refresh", async (t) => {
  const f = await fixture(t);
  const job = await claim(f);
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  let refreshing = false;
  f.github.pr = async () => {
    refreshing = true;
    await gate;
    return pr(1, { head: { sha: "d".repeat(40) } });
  };
  t.after(() => release());
  const failure = f.worker("failed", {
    id: job.id, lease: job.lease, kind: "superseded",
  });
  await until(() => refreshing);
  await f.call("/admin/pause", { repo: "owner/project", number: 1 });
  release();
  assert.deepEqual((await failure).data, { cancel: true });
  assert.equal(f.store.get("jobs", job.id).state, "paused");
  assert.equal(f.store.all("jobs").length, 1);
});

test("inclusive catch-up waits for a normal scan and includes the excluded backlog", async (t) => {
  const f = await fixture(t);
  const repo = f.store.get("repos", "owner/project");
  f.store.enroll({ ...repo, excluded: [1] });
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  let scans = 0, active = 0, maximum = 0;
  f.github.prs = async () => {
    scans++;
    maximum = Math.max(maximum, ++active);
    if (scans === 1) await gate;
    active--;
    return [pr()];
  };
  t.after(() => release());
  const normal = f.svc.admin("catch-up", { repo: repo.name });
  await until(() => scans === 1);
  const inclusive = f.svc.admin("catch-up", { repo: repo.name, includeBacklog: true });
  const duplicate = f.svc.admin("catch-up", { repo: repo.name, includeBacklog: true });
  release();
  assert.equal((await normal)[repo.name].queued, 0);
  assert.equal((await inclusive)[repo.name].queued, 1);
  assert.equal((await duplicate)[repo.name].queued, 1);
  assert.equal(scans, 2);
  assert.equal(maximum, 1);
  assert.deepEqual(f.store.get("repos", repo.name).excluded, []);
  assert.equal(f.store.all("jobs").length, 1);
});
