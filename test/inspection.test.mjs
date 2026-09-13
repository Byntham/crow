import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { processRun, cleanEnv } from "../dist/lib/util.mjs";
import {
  git,
  revision,
  safePath,
  files,
  readBlob,
  diff,
  guidance,
  inspectionTool,
} from "../dist/lib/inspection.mjs";
async function fixture(t) {
  const dir = await mkdtemp(join(tmpdir(), "crow-inspection-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const run = (args) =>
    processRun("git", ["-C", dir, ...args], {
      env: cleanEnv({
        GIT_AUTHOR_NAME: "Fixture",
        GIT_AUTHOR_EMAIL: "fixture@example.test",
        GIT_COMMITTER_NAME: "Fixture",
        GIT_COMMITTER_EMAIL: "fixture@example.test",
      }),
    });
  await run(["init", "-b", "main"]);
  await mkdir(join(dir, "src"));
  await mkdir(join(dir, ".crow"));
  await writeFile(join(dir, "AGENTS.md"), "Trusted root rules.");
  await writeFile(join(dir, ".crow", "review.md"), "Trusted review rules.");
  await writeFile(join(dir, "src", "AGENTS.md"), "Scoped source rules.");
  await writeFile(join(dir, "src", "api.js"), "const old = 1;\n");
  await writeFile(join(dir, "binary.dat"), Buffer.from([0, 1, 2]));
  await symlink("/etc/passwd", join(dir, "outside"));
  await run(["add", "."]);
  await run(["commit", "-m", "base"]);
  const base = (await run(["rev-parse", "HEAD"])).stdout.trim();
  await run(["checkout", "-b", "feature"]);
  await writeFile(
    join(dir, "src", "api.js"),
    "const old = 1;\nconst changed = 2;\n",
  );
  await writeFile(join(dir, "AGENTS.md"), "Untrusted changed rules.");
  await writeFile(join(dir, ".crow", "review.md"), "Untrusted review rules.");
  await writeFile(join(dir, "$(touch SHOULD_NOT_EXIST).txt"), "literal-name");
  await run(["add", "."]);
  await run(["commit", "-m", "feature"]);
  const head = (await run(["rev-parse", "HEAD"])).stdout.trim();
  await run(["checkout", "main"]);
  await writeFile(join(dir, "unrelated.txt"), "unrelated base advancement");
  await run(["add", "."]);
  await run(["commit", "-m", "main advancement"]);
  const targetSha = (await run(["rev-parse", "HEAD"])).stdout.trim();
  // Copy only Git objects into a bare repository. No reviewed files become executable working files.
  const bare = join(dir, "bare.git");
  await run(["clone", "--bare", dir, bare]);
  return { source: { dir: bare, base, head, targetSha, target: "main" }, dir };
}
test("revision and repository path validators reject option and traversal injection", () => {
  assert.equal(revision("a".repeat(40)), "a".repeat(40));
  for (const value of [
    "HEAD",
    "--help",
    "a".repeat(39),
    "a".repeat(40) + ";echo x",
  ])
    assert.throws(() => revision(value));
  for (const value of [
    "/etc/passwd",
    "../secret",
    "x/../../secret",
    "x\\secret",
    "x\0y",
    "x\ny",
    "a//b",
  ])
    assert.throws(() => safePath(value));
  assert.equal(safePath("$(touch file).txt"), "$(touch file).txt");
});
test("inspection reads pinned blobs in bare Git without following symlinks", async (t) => {
  const { source } = await fixture(t);
  assert.match(await readBlob(source, "src/api.js"), /changed/);
  assert.equal(
    await readBlob(source, "src/api.js", source.base),
    "const old = 1;\n",
  );
  await assert.rejects(readBlob(source, "outside"), /regular tracked files/);
  await assert.rejects(readBlob(source, "binary.dat"), /Binary/);
  await assert.rejects(readBlob(source, "missing"), /regular tracked files/);
  assert.equal(
    (await git(source.dir, ["rev-parse", "--is-bare-repository"])).trim(),
    "true",
  );
});
test("comparison uses merge base and excludes unrelated target commits", async (t) => {
  const { source } = await fixture(t);
  assert.equal(
    (
      await git(source.dir, ["merge-base", source.head, source.targetSha])
    ).trim(),
    source.base,
  );
  const patch = await diff(source);
  assert.match(patch, /const changed = 2/);
  assert.doesNotMatch(patch, /unrelated base advancement/);
});
test("guidance comes exclusively from pinned target branch", async (t) => {
  const { source } = await fixture(t);
  const rules = await guidance(source);
  assert.deepEqual(
    rules.files.map((f) => f.path),
    [".crow/review.md", "AGENTS.md", "src/AGENTS.md"],
  );
  assert.match(JSON.stringify(rules.files), /Trusted root rules/);
  assert.doesNotMatch(JSON.stringify(rules.files), /Untrusted/);
  assert.equal(rules.fingerprint, (await guidance(source)).fingerprint);
});
test("literal filename and search strings never execute shell syntax", async (t) => {
  const { source } = await fixture(t);
  assert.equal(
    await readBlob(source, "$(touch SHOULD_NOT_EXIST).txt"),
    "literal-name",
  );
  assert.ok((await files(source)).includes("$(touch SHOULD_NOT_EXIST).txt"));
  assert.equal(
    await inspectionTool(source, "search", {
      text: "$(touch SHOULD_NOT_EXIST)",
    }),
    "",
  );
  assert.match(
    await inspectionTool(source, "search", {
      text: "changed",
      path: "src/api.js",
    }),
    /const changed/,
  );
  await assert.rejects(
    inspectionTool(source, "search", { text: "" }),
    /Search needs/,
  );
});
test("inspection tools expose bounded line reads and reject execution tools", async (t) => {
  const { source } = await fixture(t);
  assert.equal(
    await inspectionTool(source, "read_file", {
      path: "src/api.js",
      start: 2,
      count: 1,
    }),
    "2: const changed = 2;",
  );
  assert.deepEqual(
    await inspectionTool(source, "list_files", { prefix: "src/" }),
    ["src/AGENTS.md", "src/api.js"],
  );
  await assert.rejects(
    inspectionTool(source, "shell", { command: "npm test" }),
    /Unknown inspection tool/,
  );
});

test("diff does not invoke a configured external diff command", async (t) => {
  const { source, dir } = await fixture(t);
  const canary = join(dir, "external-diff-ran");
  const command = join(dir, "external-diff.sh");
  await writeFile(command, `#!/bin/sh\ntouch '${canary}'\n`, { mode: 0o700 });
  await git(source.dir, ["config", "diff.external", command]);
  assert.match(await diff(source), /const changed/);
  const { access } = await import("node:fs/promises");
  await assert.rejects(access(canary), (e) => e.code === "ENOENT");
});
