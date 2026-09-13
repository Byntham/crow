import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { defaults } from "../dist/lib/config.mjs";
import { workerRequest } from "../dist/lib/worker.mjs";

async function service(t, value) {
  const server = createServer((_request, response) => {
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify(value));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const config = defaults("/tmp/crow-protocol-test");
  config.serviceUrl = `http://127.0.0.1:${server.address().port}`;
  return config;
}

function claim() {
  return {
    job: {
      id: "review-1",
      key: "owner/repo#1",
      repo: "owner/repo",
      number: 1,
      head: "a".repeat(40),
      target: "main",
      worker: "worker-1",
      state: "reviewing",
      manual: false,
      restart: false,
      priority: 0,
      resumeEpoch: 0,
      createdAt: 1,
      updatedAt: 1,
      retries: 0,
      nextAt: 0,
      lease: "lease-1",
    },
    pr: {
      number: 1,
      state: "open",
      draft: false,
      user: { login: "owner" },
      head: { sha: "a".repeat(40) },
      base: { sha: "b".repeat(40), ref: "main" },
      title: "A change",
      body: null,
    },
    token: "installation-token",
  };
}

test("worker accepts validated claims and empty queues", async (t) => {
  const work = claim();
  assert.deepEqual(await workerRequest(await service(t, work), "next"), work);
  assert.equal(await workerRequest(await service(t, null), "next"), null);
});

test("worker rejects malformed service responses before executing work", async (t) => {
  const cases = [
    ["nonstring installation token", "next", { ...claim(), token: 42 }],
    [
      "nonnumber PR identifier",
      "next",
      { ...claim(), job: { ...claim().job, number: "1" } },
    ],
    [
      "unknown job state",
      "next",
      { ...claim(), job: { ...claim().job, state: "invented" } },
    ],
    [
      "missing PR head",
      "next",
      { ...claim(), pr: { ...claim().pr, head: null } },
    ],
    [
      "incomplete report",
      "next",
      {
        ...claim(),
        job: { ...claim().job, report: { summary: "incomplete" } },
      },
    ],
    [
      "unknown retention state",
      "maintenance",
      {
        jobs: [{ id: "review-1", state: "invented", updatedAt: 1 }],
        retentionDays: 7,
      },
    ],
    ["nonarray retention jobs", "maintenance", { jobs: {}, retentionDays: 7 }],
    ["nonboolean cancellation", "heartbeat", { cancel: "false" }],
    ["nonboolean skip", "comparison", { skip: 1 }],
    ["missing worker identifier", "ping", { ok: true, id: null }],
  ];
  for (const [label, action, value] of cases) {
    await t.test(label, async (t) => {
      await assert.rejects(workerRequest(await service(t, value), action));
    });
  }
});
