import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  writeFile,
  readFile,
  symlink,
  lstat,
  rm,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { cleanup } from "../dist/lib/retention.mjs";
const now = Date.now();
const old = now - 8 * 86400000;
const session = "11111111-2222-3333-4444-555555555555";
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-retention-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}
async function file(path) {
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, "keep");
  return path;
}
async function exists(path) {
  return lstat(path).then(
    () => true,
    (e) => {
      if (e.code === "ENOENT") return false;
      throw e;
    },
  );
}

test("retention deletes expired terminal artifacts but preserves paused, fresh and unknown jobs", async (t) => {
  const root = await fixture(t);
  const jobs = [
    { id: "done", state: "completed", updatedAt: old, session },
    { id: "paused", state: "paused", updatedAt: old },
    { id: "fresh", state: "completed", updatedAt: now },
    { id: "unknown", state: "completed" },
  ];
  for (const job of jobs) {
    await file(join(root, "sources", job.id, "source.txt"));
    await file(join(root, "reviews", job.id, "session.json"));
    await file(join(root, "reports", `${job.id}.json`));
    await file(join(root, "logs", `${job.id}.log`));
  }
  const rollout = await file(
    join(
      root,
      "codex",
      "sessions",
      "2026",
      "09",
      "13",
      `rollout-2026-09-13T00-00-00-${session}.jsonl`,
    ),
  );
  const other = await file(
    join(root, "codex", "sessions", "rollout-other.jsonl"),
  );
  const auth = await file(join(root, "codex", "auth.json"));
  const db = await file(join(root, "codex", "state_5.sqlite"));
  assert.deepEqual(await cleanup(root, jobs, 7, { now }), {
    removed: ["done"],
    warnings: [],
  });
  for (const folder of ["sources", "reviews"]) {
    assert.equal(await exists(join(root, folder, "done")), false);
    for (const id of ["paused", "fresh", "unknown"])
      assert.equal(await exists(join(root, folder, id)), true);
  }
  assert.equal(await exists(rollout), false);
  for (const path of [other, auth, db]) assert.equal(await exists(path), true);
});

test("retention unlinks job symlinks and refuses linked parent directories", async (t) => {
  const root = await fixture(t),
    external = await fixture(t);
  const outside = await file(join(external, "valuable.txt"));
  await mkdir(join(root, "sources"));
  await symlink(external, join(root, "sources", "done"));
  await symlink(external, join(root, "reviews"));
  const result = await cleanup(
    root,
    [{ id: "done", state: "cancelled", updatedAt: old }],
    7,
    { now },
  );
  assert.equal(await readFile(outside, "utf8"), "keep");
  assert.equal(await exists(join(root, "sources", "done")), false);
  assert.equal(result.warnings.length, 1);
});

test("retention never follows a linked Codex home or purges sessions used by unfinished jobs", async (t) => {
  const root = await fixture(t),
    external = await fixture(t);
  const rollout = await file(
    join(external, "sessions", `rollout-date-${session}.jsonl`),
  );
  await symlink(external, join(root, "codex"));
  const expired = {
    id: "done",
    state: "superseded",
    updatedAt: old,
    session: { id: session },
  };
  const result = await cleanup(root, [expired], 7, { now });
  assert.equal(result.warnings.length, 1);
  assert.equal(await exists(rollout), true);
  await rm(join(root, "codex"));
  const owned = await file(
    join(root, "codex", "sessions", `rollout-date-${session}.jsonl`),
  );
  await cleanup(
    root,
    [expired, { id: "paused", state: "paused", updatedAt: old, session }],
    7,
    { now },
  );
  assert.equal(await exists(owned), true);
});

test("retention ignores traversal IDs and rejects invalid retention configuration", async (t) => {
  const root = await fixture(t);
  const path = await file(join(root, "keep.txt"));
  assert.deepEqual(
    await cleanup(
      root,
      [{ id: "../keep.txt", state: "completed", updatedAt: old }],
      0,
      { now },
    ),
    { removed: [], warnings: [] },
  );
  assert.equal(await exists(path), true);
  await assert.rejects(cleanup(root, [], -1), /Retention days/);
});

test("retention collects explicit delegated reviewer sessions before deleting workspaces", async (t) => {
  const root = await fixture(t);
  const child = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  const shared = "22222222-3333-4444-5555-666666666666";
  for (const [job, task, id] of [
    ["done", "child", child],
    ["done", "shared", shared],
    ["paused", "shared", shared],
  ]) {
    const record = await file(
      join(root, "reviews", job, "tasks", `${task}.json`),
    );
    await writeFile(record, JSON.stringify({ id: task, session: id }));
  }
  const rollout = await file(
    join(root, "codex", "sessions", `rollout-date-${child}.jsonl`),
  );
  const kept = await file(
    join(root, "codex", "archived_sessions", `rollout-date-${shared}.jsonl`),
  );
  await cleanup(
    root,
    [
      { id: "done", state: "completed", updatedAt: old },
      { id: "paused", state: "paused", updatedAt: old },
    ],
    7,
    { now },
  );
  assert.equal(await exists(rollout), false);
  assert.equal(await exists(kept), true);
  assert.equal(await exists(join(root, "reviews", "done")), false);
  assert.equal(await exists(join(root, "reviews", "paused")), true);
});
