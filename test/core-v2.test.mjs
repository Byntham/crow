import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createHmac,
  generateKeyPairSync,
  verify as verifySignature,
} from "node:crypto";
import {
  Store,
  eligible,
  comparisonKey,
  retryDelay,
} from "../dist/lib/store.mjs";
import {
  validateReport,
  reportBody,
  marker,
  metadata,
  inlineComments,
} from "../dist/lib/report.mjs";
import { GitHub, verify, jwt } from "../dist/lib/github.mjs";
import {
  equal,
  atomic,
  json,
  processRun,
  repoName,
  httpsUrl,
  cleanEnv,
} from "../dist/lib/util.mjs";

const repo = {
  name: "owner/project",
  worker: "local",
  policy: "selected",
  authors: ["Alice"],
};
const pr = (sha = "a".repeat(40), extra = {}) => ({
  number: 3,
  state: "open",
  draft: false,
  user: { login: "alice" },
  head: { sha },
  base: { ref: "main" },
  ...extra,
});
async function database(t) {
  const dir = await mkdtemp(join(tmpdir(), "crow-core-"));
  const file = join(dir, "state.sqlite");
  const s = new Store(file);
  t.after(async () => {
    try {
      s.close();
    } catch {}
    await rm(dir, { recursive: true, force: true });
  });
  return { s, file, dir };
}
const finding = {
  title: "Missing authorization",
  body: "The handler permits another account to read this record.",
  path: "src/api.js",
  line: 2,
  severity: "high",
};
const report = () =>
  validateReport({ summary: "One correctness issue.", findings: [finding] });

test("delivery receipts and pending events survive close and reopening", async (t) => {
  const { s, file } = await database(t);
  assert.equal(
    s.acceptEvent("delivery-1", { type: "pull_request", number: 3 }),
    true,
  );
  s.close();
  const next = new Store(file);
  try {
    assert.deepEqual(next.events(), [
      { id: "delivery-1", type: "pull_request", number: 3 },
    ]);
    assert.equal(next.acceptEvent("delivery-1", { type: "duplicate" }), false);
    next.eventDone("delivery-1");
    assert.deepEqual(next.events(), []);
    assert.equal(next.acceptEvent("delivery-1", { type: "duplicate" }), false);
  } finally {
    next.close();
  }
});
test("a delivery persisted immediately before abrupt process exit is replayable", async (t) => {
  const { s, file } = await database(t);
  s.close();
  const module = new URL("../dist/lib/store.mjs", import.meta.url).href;
  await assert.rejects(
    processRun(process.execPath, [
      "--input-type=module",
      "-e",
      `import {Store} from ${JSON.stringify(module)};const s=new Store(process.argv[1]);s.acceptEvent('crashed',{type:'pull_request'});process.exit(7);`,
      file,
    ]),
    (e) => e.code === 7,
  );
  const next = new Store(file);
  try {
    assert.equal(next.events()[0].id, "crashed");
    assert.equal(next.acceptEvent("crashed", {}), false);
  } finally {
    next.close();
  }
});
test("failed event serialization rolls back its receipt", async (t) => {
  const { s } = await database(t);
  const circular = {};
  circular.self = circular;
  assert.throws(() => s.acceptEvent("delivery", circular));
  assert.equal(s.acceptEvent("delivery", { type: "pull_request" }), true);
});
test("queue coalesces repeated events and supersedes older revisions", async (t) => {
  const { s } = await database(t);
  const first = s.queue(repo, pr());
  assert.equal(s.queue(repo, pr()).id, first.id);
  const next = s.queue(repo, pr("b".repeat(40)));
  assert.notEqual(next.id, first.id);
  assert.equal(s.get("jobs", first.id).state, "superseded");
  assert.equal(s.claim("other"), null);
  assert.equal(s.claim("local").id, next.id);
  assert.equal(s.claim("local"), null);
});
test("competing store connections cannot claim the same job", async (t) => {
  const { s, file } = await database(t);
  s.queue(repo, pr());
  const next = new Store(file);
  try {
    assert.ok(s.claim("local"));
    assert.equal(next.claim("local"), null);
  } finally {
    next.close();
  }
});
test("an obsolete lease cannot publish or modify a superseded job", async (t) => {
  const { s } = await database(t);
  s.queue(repo, pr());
  const old = s.claim("local");
  s.queue(repo, pr("b".repeat(40)));
  assert.throws(
    () => s.updateJob(old.id, { report: report() }, old.lease),
    /ownership lost/,
  );
});
test("manual request resumes paused session and resets retry wait", async (t) => {
  const { s } = await database(t);
  const first = s.queue(repo, pr());
  s.updateJob(first.id, {
    state: "paused",
    session: "session-123",
    retries: 10,
    nextAt: Date.now() + 60000,
  });
  const resumed = s.queue(repo, pr(), { manual: true });
  assert.equal(resumed.id, first.id);
  assert.equal(resumed.state, "queued");
  assert.equal(resumed.session, "session-123");
  assert.equal(resumed.retries, 0);
  assert.equal(resumed.nextAt, 0);
});
test("a held backlog job starts on manual request without needing a nonexistent session", async (t) => {
  const { s } = await database(t);
  const held = s.queue(repo, pr(), { held: true });
  assert.equal(s.claim("local"), null);
  const released = s.queue(repo, pr(), { manual: true });
  assert.equal(released.id, held.id);
  assert.equal(released.state, "queued");
  assert.equal(s.claim("local").id, held.id);
});
test("paused review without saved state requires explicit restart", async (t) => {
  const { s } = await database(t);
  s.queue(repo, pr());
  const job = s.claim("local");
  s.updateJob(job.id, { state: "paused" });
  const resume = s.queue(repo, pr(), { manual: true });
  assert.equal(resume.state, "paused");
  assert.match(resume.reason, /restart required/i);
  const restart = s.queue(repo, pr(), { manual: true, restart: true });
  assert.notEqual(restart.id, job.id);
  assert.equal(restart.state, "queued");
});
test("validated report resumes comparison verification without a provider session", async (t) => {
  const { s } = await database(t);
  const job = s.queue(repo, pr());
  s.updateJob(job.id, { state: "paused", report: report() });
  assert.equal(s.queue(repo, pr(), { manual: true }).state, "queued");
});
test("explicit review of completed comparison creates a new review", async (t) => {
  const { s } = await database(t);
  const job = s.queue(repo, pr());
  s.updateJob(job.id, { state: "completed" });
  assert.notEqual(s.queue(repo, pr(), { manual: true }).id, job.id);
});
test("retrying jobs cannot be claimed before their delay expires", async (t) => {
  const { s } = await database(t);
  const job = s.queue(repo, pr());
  s.updateJob(job.id, { state: "retrying", nextAt: Date.now() + 60000 });
  assert.equal(s.claim("local"), null);
  s.updateJob(job.id, { nextAt: 0 });
  assert.ok(s.claim("local"));
});
test("eligibility uses target author policy regardless of source fork", () => {
  assert.equal(eligible(repo, pr()), true);
  assert.equal(eligible(repo, pr(undefined, { draft: true })), false);
  assert.equal(eligible(repo, pr(undefined, { state: "closed" })), false);
  assert.equal(
    eligible(repo, pr(undefined, { user: { login: "mallory" } })),
    false,
  );
  assert.equal(
    eligible(
      { ...repo, policy: "everyone" },
      pr(undefined, {
        user: { login: "mallory" },
        head: { sha: "a".repeat(40), repo: { fork: true } },
      }),
    ),
    true,
  );
});
test("comparison freshness ignores target tip but includes merge base and target branch", () => {
  const c = { head: "a", base: "b", target: "main", targetSha: "c" };
  assert.equal(comparisonKey(c), comparisonKey({ ...c, targetSha: "d" }));
  assert.notEqual(comparisonKey(c), comparisonKey({ ...c, base: "d" }));
  assert.notEqual(comparisonKey(c), comparisonKey({ ...c, target: "release" }));
});
test("fixed and progressive retry delays respect requested provider cooldown", () => {
  assert.equal(
    retryDelay({ mode: "fixed", delayMs: 5000 }, 10, 0, () => 0),
    5000,
  );
  assert.deepEqual(
    [1, 2, 3, 4, 5, 6, 7].map((n) =>
      retryDelay({ mode: "progressive" }, n, 0, () => 0),
    ),
    [5000, 15000, 30000, 60000, 120000, 300000, 300000],
  );
  assert.equal(
    retryDelay({ mode: "fixed", delayMs: 5000 }, 1, 60000, () => 0),
    60000,
  );
  assert.equal(
    retryDelay({ mode: "fixed", delayMs: 5000 }, 1, 0, () => 0.5),
    5250,
  );
});

test("report validation fails closed for malformed final output", () => {
  for (const value of [
    null,
    {},
    { summary: "", findings: [] },
    { summary: "ok", findings: {} },
    { summary: "ok", findings: [{ ...finding, line: 0 }] },
    { summary: "ok", findings: [{ ...finding, path: "../secret" }] },
    { summary: "ok", findings: [{ ...finding, severity: "info" }] },
    { summary: "ok", findings: [{ ...finding, severity: ["high"] }] },
    { summary: "ok", findings: Array(101).fill(finding) },
  ])
    assert.throws(() => validateReport(value));
  assert.deepEqual(
    validateReport({ summary: "Reviewed; no issues.", findings: [] }),
    { summary: "Reviewed; no issues.", findings: [] },
  );
});
test("finding identity is stable across line movement", () => {
  const previous = report().findings[0];
  const moved = validateReport({
    summary: "Still present.",
    findings: [{ ...finding, line: 20 }],
  }).findings[0];
  assert.equal(previous.id, moved.id);
});
test("completion metadata roundtrips exact comparison and ignores invalid markers", () => {
  const meta = {
    head: "a".repeat(40),
    base: "b".repeat(40),
    target: "main",
    job: "job1",
  };
  assert.deepEqual(metadata(`text\n${marker(meta)}`), meta);
  assert.equal(metadata("ordinary comment"), null);
  assert.equal(metadata("<!-- crow-review:v1 eA -->"), null);
});
test("summary preserves earlier unassessed findings and current details", () => {
  const current = report();
  const job = {
    id: "job1",
    repo: repo.name,
    comparison: { head: "a".repeat(40), base: "b".repeat(40), target: "main" },
    settings: { model: "provider-model", effort: "high" },
    guidanceFingerprint: "guidance",
    report: current,
  };
  const body = reportBody(job, [
    {
      id: "old",
      title: "Earlier problem",
      url: "https://github.com/owner/project/pull/3#discussion_r1",
    },
    {
      ...current.findings[0],
      url: "https://github.com/owner/project/pull/3#discussion_r2",
    },
  ]);
  assert.match(body, /Earlier findings, not reassessed/);
  assert.match(body, /Earlier problem/);
  assert.match(body, /Missing authorization/);
  assert.match(body, /src\/api.js:2/);
  assert.equal(metadata(body).base, job.comparison.base);
  assert.equal(metadata(body).head, job.comparison.head);
  assert.doesNotMatch(body, /discussion_r2/);
});
test("inline comments anchor only introduced lines and avoid duplicate findings", () => {
  const patch =
    "diff --git a/src/api.js b/src/api.js\n--- a/src/api.js\n+++ b/src/api.js\n@@ -1,2 +1,3 @@\n context\n+new\n tail\n";
  const r = report();
  assert.deepEqual(
    inlineComments(r, patch).map((c) => [c.path, c.line, c.side]),
    [["src/api.js", 2, "RIGHT"]],
  );
  assert.deepEqual(inlineComments(r, patch, r.findings), []);
  assert.deepEqual(
    inlineComments({ ...r, findings: [{ ...r.findings[0], line: 1 }] }, patch),
    [],
  );
});

test("webhook authentication checks exact bytes and rejects missing signatures", () => {
  const body = Buffer.from('{"event":"push"}'),
    secret = "private-secret",
    sig = `sha256=${createHmac("sha256", secret).update(body).digest("hex")}`;
  assert.equal(verify(body, sig, secret), true);
  assert.equal(
    verify(Buffer.concat([body, Buffer.from(" ")]), sig, secret),
    false,
  );
  assert.equal(verify(body, undefined, secret), false);
  assert.equal(verify(body, sig, ""), false);
});
test("constant time comparison rejects unequal encoded byte lengths without throwing", () => {
  assert.equal(equal("a", "é"), false);
  assert.equal(equal("token", "token"), true);
  assert.equal(equal(undefined, "token"), false);
});
test("GitHub App JWT is signed and has short expiry", () => {
  const { privateKey, publicKey } = generateKeyPairSync("rsa", {
    modulusLength: 2048,
  });
  const token = jwt({ id: 123, pem: privateKey });
  const [h, p, s] = token.split(".");
  const claims = JSON.parse(Buffer.from(p, "base64url"));
  assert.equal(claims.iss, "123");
  assert.ok(claims.exp - claims.iat <= 600);
  assert.equal(
    verifySignature(
      "RSA-SHA256",
      Buffer.from(`${h}.${p}`),
      publicKey,
      Buffer.from(s, "base64url"),
    ),
    true,
  );
});
test("published reviews are advisory and pinned to actual reviewed commit", async () => {
  let request;
  const gh = new GitHub(
    {},
    {
      fetcher: async (url, opts) => {
        request = { url, ...opts };
        return Response.json({
          id: 1,
          html_url:
            "https://github.com/owner/project/pull/3#pullrequestreview-1",
        });
      },
    },
  );
  await gh.publish(repo, 3, "secret", "body", "a".repeat(40), []);
  assert.deepEqual(JSON.parse(request.body), {
    commit_id: "a".repeat(40),
    event: "COMMENT",
    body: "body",
    comments: [],
  });
  assert.match(request.url, /\/pulls\/3\/reviews$/);
});
test("status discovery never edits another bot comment", async () => {
  const requests = [];
  const gh = new GitHub(
    {},
    {
      fetcher: async (url, opts) => {
        requests.push({ url, ...opts });
        if (opts.method === "GET")
          return Response.json([
            { id: 1, user: { id: 99 }, body: "<!-- crow-status:v1 --> fake" },
            { id: 2, user: { id: 42 }, body: "<!-- crow-status:v1 --> real" },
          ]);
        return Response.json({ id: 2 });
      },
    },
  );
  await gh.status(repo, 3, "token", "updated", 42);
  assert.equal(requests[1].method, "PATCH");
  assert.match(requests[1].url, /issues\/comments\/2$/);
});
test("GitHub request errors omit response bodies and retain retry delay", async () => {
  const gh = new GitHub(
    {},
    {
      fetcher: async () =>
        new Response("secret detail", {
          status: 429,
          headers: { "retry-after": "20" },
        }),
    },
  );
  await assert.rejects(
    gh.request("/repos/owner/project"),
    (e) =>
      e.status === 429 &&
      e.retryAfter === 20000 &&
      !e.message.includes("secret"),
  );
});
test("GitHub collection pagination includes complete list", async () => {
  let calls = 0;
  const gh = new GitHub(
    {},
    {
      fetcher: async () =>
        Response.json(
          ++calls === 1
            ? Array.from({ length: 100 }, (_, id) => ({ id }))
            : [{ id: 100 }],
        ),
    },
  );
  assert.equal(
    (await gh.list("/repos/owner/project/pulls?state=open", "token")).length,
    101,
  );
  assert.equal(calls, 2);
});
test("atomic private state replaces complete values and handles missing files", async (t) => {
  const { dir } = await database(t);
  const path = join(dir, "private.json");
  await atomic(path, { a: 1 });
  await atomic(path, { a: 2 });
  assert.deepEqual(await json(path), { a: 2 });
  assert.equal((await stat(path)).mode & 0o777, 0o600);
  assert.equal(await json(join(dir, "missing"), null), null);
});
test("subprocess command arguments stay literal and output bounds are enforced", async () => {
  const text = "$(echo injected); `echo injected`";
  assert.equal(
    (
      await processRun(process.execPath, [
        "-e",
        "process.stdout.write(process.argv[1])",
        text,
      ])
    ).stdout,
    text,
  );
  await assert.rejects(
    processRun(
      process.execPath,
      ["-e", 'process.stdout.write("x".repeat(10000))'],
      { limit: 100 },
    ),
    /output limit/,
  );
});
test("child environment excludes ambient provider keys and shell overrides", () => {
  const env = cleanEnv();
  assert.equal(env.OPENAI_API_KEY, undefined);
  assert.equal(env.BASH_ENV, undefined);
  assert.equal(env.GIT_CONFIG_GLOBAL, "/dev/null");
});
test("repository and HTTPS inputs cannot insert paths or credentials", () => {
  assert.equal(repoName("Alice/Project"), "alice/project");
  for (const invalid of [
    "../project",
    "owner/../repo",
    "https://github.com/owner/repo",
    "owner/repo?x=1",
  ])
    assert.throws(() => repoName(invalid));
  assert.equal(
    httpsUrl("https://host.example:8443"),
    "https://host.example:8443",
  );
  for (const invalid of [
    "http://host.example",
    "https://secret@host.example",
    "https://host.example/path",
    "https://host.example?token=secret",
  ])
    assert.throws(() => httpsUrl(invalid));
});

test("cancelled subprocesses stop and preserve the requested cancellation reason", async () => {
  const controller = new AbortController();
  const reason = new Error("operator paused review");
  const task = processRun(
    process.execPath,
    ["-e", "setInterval(()=>{},1000)"],
    { signal: controller.signal },
  );
  controller.abort(reason);
  await assert.rejects(task, (e) => e === reason);
});
test("subprocess timeout bounds a silent stalled tool", async () => {
  await assert.rejects(
    processRun(process.execPath, ["-e", "setInterval(()=>{},1000)"], {
      timeout: 30,
    }),
    /timed out/,
  );
});

test("webhook audit follows cursor links and retries failed connections but not recovered deliveries", async () => {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const calls = [];
  const now = new Date().toISOString();
  const gh = new GitHub(
    { id: 1, pem: privateKey },
    {
      fetcher: async (url, opts) => {
        calls.push({ url, method: opts.method });
        if (opts.method === "POST")
          return Response.json({ id: 1 }, { status: 202 });
        if (new URL(url).searchParams.has("cursor"))
          return Response.json([
            {
              id: 3,
              guid: "recovered",
              status_code: 200,
              redelivery: true,
              delivered_at: now,
            },
            {
              id: 4,
              guid: "http",
              status_code: 500,
              redelivery: false,
              delivered_at: now,
            },
          ]);
        return Response.json(
          [
            {
              id: 1,
              guid: "network",
              status_code: 0,
              redelivery: false,
              delivered_at: now,
            },
            {
              id: 2,
              guid: "recovered",
              status_code: 500,
              redelivery: false,
              delivered_at: now,
            },
          ],
          {
            headers: {
              link: '<https://api.github.com/app/hook/deliveries?per_page=100&cursor=next-cursor>; rel="next"',
            },
          },
        );
      },
    },
  );
  await gh.audit();
  assert.deepEqual(
    calls
      .filter((x) => x.method === "POST")
      .map((x) => new URL(x.url).pathname),
    ["/app/hook/deliveries/1/attempts", "/app/hook/deliveries/4/attempts"],
  );
  assert.ok(calls.some((x) => x.url.includes("cursor=next-cursor")));
  assert.ok(calls.every((x) => !x.url.includes("&page=")));
});
test("delivery pagination does not leak App tokens to an external next-page URL", async () => {
  const gh = new GitHub(
    {},
    {
      fetcher: async () =>
        Response.json([], {
          headers: {
            link: '<https://attacker.example/app/hook/deliveries?cursor=secret>; rel="next"',
          },
        }),
    },
  );
  await assert.rejects(gh.deliveries("app-bearer-token"), /pagination origin/);
});

test("summary links earlier reports when individual finding history is unavailable", () => {
  const job = {
    id: "job1",
    repo: repo.name,
    comparison: { head: "a".repeat(40), base: "b".repeat(40), target: "main" },
    settings: { model: "provider-model", effort: "high" },
    report: { summary: "No new issues.", findings: [] },
  };
  const body = reportBody(
    job,
    [],
    [
      {
        id: 123,
        html_url:
          "https://github.com/owner/project/pull/3#pullrequestreview-123",
      },
    ],
  );
  assert.match(body, /Earlier reviews, findings not reassessed:/);
  assert.match(body, /pullrequestreview-123/);
  assert.doesNotMatch(body, /resolved|fixed/);
});

test("oversized complete reports fail validation as correctable output before publication", () => {
  const value = {
    summary: "Complete analysis.",
    findings: Array.from({ length: 12 }, (_, i) => ({
      ...finding,
      title: `Issue ${i}`,
      body: "x".repeat(4000),
    })),
  };
  assert.throws(
    () => validateReport(value),
    (e) => e.kind === "output" && /shorten/.test(e.message),
  );
});
test("newline-heavy findings cannot exceed the rendered publication budget", () => {
  const value = {
    summary: "Complete analysis.",
    findings: Array.from({ length: 5 }, (_, i) => ({
      ...finding,
      title: `Issue ${i}`,
      body: "x" + "\n".repeat(3999),
    })),
  };
  assert.throws(
    () => validateReport(value),
    (e) => e.kind === "output",
  );
});
test("large finding history collapses to prior report links without dropping current findings", () => {
  const job = {
    id: "job1",
    number: 3,
    repo: repo.name,
    comparison: { head: "a".repeat(40), base: "b".repeat(40), target: "main" },
    settings: { model: "provider-model", effort: "high" },
    report: report(),
  };
  const previous =
    "https://github.com/owner/project/pull/3#pullrequestreview-123";
  const history = Array.from({ length: 1000 }, (_, i) => ({
    id: `old-${i}`,
    title: "Earlier issue ".repeat(20),
    url: previous,
  }));
  const body = reportBody(job, history);
  assert.ok(body.length <= 60000);
  assert.match(body, /Missing authorization/);
  assert.match(body, /Earlier reviews, findings not reassessed/);
  assert.ok(body.includes(previous));
});
test("unbounded review history retains a previous-review and full-history link within GitHub limit", () => {
  const job = {
    id: "job1",
    number: 3,
    repo: repo.name,
    comparison: { head: "a".repeat(40), base: "b".repeat(40), target: "main" },
    settings: { model: "provider-model", effort: "high" },
    report: report(),
  };
  const prior = Array.from({ length: 2000 }, (_, i) => ({
    url: `https://github.com/owner/project/pull/3#pullrequestreview-${i}`,
  }));
  const body = reportBody(job, [], prior);
  assert.ok(body.length <= 60000);
  assert.match(body, /Earlier findings not reassessed/);
  assert.match(body, /pullrequestreview-1999/);
  assert.match(body, /PR review history/);
  assert.match(body, /Missing authorization/);
});

test("streaming provider events do not accumulate an output-size runtime cutoff", async () => {
  let lines = 0;
  const output = await processRun(
    process.execPath,
    ["-e", 'process.stdout.write(("event\\n").repeat(10000))'],
    {
      capture: false,
      limit: 100,
      onLine: () => lines++,
    },
  );
  assert.equal(lines, 10000);
  assert.equal(output.stdout, "");
  await assert.rejects(
    processRun(
      process.execPath,
      ["-e", 'process.stdout.write("x".repeat(1000))'],
      {
        capture: false,
        limit: 100,
        onLine: () => {},
      },
    ),
    /line limit/,
  );
});

test("retention compacts terminal reports without deleting pending receipts or paused work", async (t) => {
  const { s } = await database(t);
  const old = Date.now() - 10 * 86400000;
  s.put("jobs", "done", {
    id: "done",
    state: "completed",
    updatedAt: old,
    session: "session",
    report: { summary: "details" },
    patch: "large diff",
    comparison: { head: "a" },
    reviewUrl: "https://github.com/report",
  });
  s.put("jobs", "paused", {
    id: "paused",
    state: "paused",
    updatedAt: old,
    report: { summary: "saved" },
  });
  s.acceptEvent("pending", { type: "ping" });
  s.db.prepare("UPDATE receipts SET received=?").run(old);
  s.prune(7);
  assert.equal(s.get("jobs", "done").report, undefined);
  assert.equal(s.get("jobs", "done").patch, undefined);
  assert.equal(s.get("jobs", "done").updatedAt, old);
  assert.equal(s.get("jobs", "done").session, "session");
  assert.equal(s.get("jobs", "paused").report.summary, "saved");
  assert.equal(s.acceptEvent("pending", { type: "ping" }), false);
});
