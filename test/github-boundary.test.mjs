import test from "node:test";
import assert from "node:assert/strict";
import { GitHub } from "../dist/lib/github.mjs";

const repo = { name: "owner/project", installation: 1 };
function github(response) {
  return new GitHub({}, { fetcher: async () => Response.json(response) });
}

test("GitHub rejects incomplete pull request responses before scheduling", async () => {
  await assert.rejects(
    github({ number: 3 }).pr(repo, 3, "token"),
    /Unexpected GitHub/,
  );
  await assert.rejects(
    github([{ number: 3 }]).prs(repo, "token"),
    /Unexpected GitHub/,
  );
});

test("GitHub rejects malformed token and publication responses", async () => {
  const { generateKeyPairSync } = await import("node:crypto");
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const gh = new GitHub(
    { id: 1, pem: privateKey },
    { fetcher: async () => Response.json({ token: 42 }) },
  );
  await assert.rejects(gh.token(repo), /Unexpected GitHub string/);
  await assert.rejects(
    github({ id: 1 }).publish(repo, 3, "token", "body", "a".repeat(40), []),
    /Unexpected GitHub string/,
  );
});

test("GitHub retains valid pull request context and normalizes empty review bodies", async () => {
  const pr = {
    number: 3,
    state: "open",
    draft: false,
    user: { login: "operator" },
    head: { sha: "a".repeat(40) },
    base: { sha: "b".repeat(40), ref: "main" },
    title: "Change",
    body: null,
  };
  assert.deepEqual(await github(pr).pr(repo, 3, "token"), pr);
  const review = {
    id: 1,
    user: { id: 2 },
    body: null,
    html_url: "https://github.com/owner/project/pull/3#pullrequestreview-1",
  };
  assert.deepEqual(await github([review]).reviews(repo, 3, "token"), [
    { ...review, body: "" },
  ]);
});

test("GitHub supports unconfigured service construction and explains missing App authentication", async () => {
  const gh = new GitHub(null, {
    fetcher: async () => Response.json({ ok: true }),
  });
  assert.deepEqual(await gh.request("/health"), { ok: true });
  await assert.rejects(gh.token(repo), /Complete GitHub App setup first/);
});

test("GitHub preserves history from deleted users without treating it as Crow's review", async () => {
  const deleted = {
    id: 1,
    user: null,
    body: "<!-- crow-review:v1 -->",
    html_url: "https://github.com/owner/project/pull/3#pullrequestreview-1",
  };
  const crow = {
    ...deleted,
    id: 2,
    user: { id: 42 },
    html_url: "https://github.com/owner/project/pull/3#pullrequestreview-2",
  };
  const reviews = await github([deleted, crow]).reviews(repo, 3, "token");
  assert.deepEqual(reviews, [deleted, crow]);
  assert.deepEqual(
    reviews.filter((review) => review.user?.id === 42),
    [crow],
  );
});

test("GitHub status discovery skips deleted comment authors and still updates Crow's comment", async () => {
  const comments = [
    { id: 1, user: null, body: "<!-- crow-status:v1 --> old" },
    { id: 2, user: { id: 42 }, body: "<!-- crow-status:v1 --> current" },
  ];
  const requests = [];
  const gh = new GitHub(null, {
    fetcher: async (url, options) => {
      requests.push({ url, method: options.method });
      return Response.json(options.method === "GET" ? comments : { id: 2 });
    },
  });
  assert.deepEqual(await gh.comments(repo, 3, "token"), comments);
  assert.deepEqual(await gh.status(repo, 3, "token", "updated", 42), { id: 2 });
  assert.equal(requests.at(-1).method, "PATCH");
  assert.match(requests.at(-1).url, /\/issues\/comments\/2$/);
});

test("GitHub normalizes omitted optional PR draft and comment body fields", async () => {
  const pr = {
    number: 3,
    state: "open",
    user: { login: "operator" },
    head: { sha: "a".repeat(40) },
    base: { sha: "b".repeat(40), ref: "main" },
  };
  assert.deepEqual(await github(pr).pr(repo, 3, "token"), {
    ...pr,
    draft: false,
  });
  assert.deepEqual(
    await github([{ id: 1, user: null }]).comments(repo, 3, "token"),
    [{ id: 1, user: null, body: "" }],
  );
  await assert.rejects(
    github({ ...pr, draft: "false" }).pr(repo, 3, "token"),
    /Unexpected GitHub boolean/,
  );
  await assert.rejects(
    github([{ id: 1, user: null, body: 42 }]).comments(repo, 3, "token"),
    /Unexpected GitHub string/,
  );
});
