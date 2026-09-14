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
  readBlob,
  diff,
  publicationPatch,
  guidance,
  inspectionTool,
} from "../dist/lib/inspection.mjs";
import { inlineComments } from "../dist/lib/report.mjs";
async function fixture(t, { largeDiff = false } = {}) {
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
  if (largeDiff) await writeFile(join(dir, "deleted.txt"), "removed by PR\n");
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
  if (largeDiff) {
    await rm(join(dir, "deleted.txt"));
    await writeFile(
      join(dir, "a-large.txt"),
      typeof largeDiff === "number"
        ? "x".repeat(largeDiff) + "\n"
        : "changed line with context\n".repeat(12000),
    );
    await writeFile(
      join(dir, "z-later.txt"),
      "late change must remain reachable\n",
    );
  }
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
  const { patch } = await diff(source);
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
  assert.ok(
    (await inspectionTool(source, "list_files", {})).files.includes(
      "$(touch SHOULD_NOT_EXIST).txt",
    ),
  );
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
    {
      files: ["src/AGENTS.md", "src/api.js"],
      offset: 0,
      total: 2,
      nextOffset: null,
      truncated: false,
    },
  );
  await assert.rejects(
    inspectionTool(source, "shell", { command: "npm test" }),
    /Unknown inspection tool/,
  );
});

test("large PR inspection exposes every changed path and every diff page", async (t) => {
  const { source } = await fixture(t, { largeDiff: true });
  const full = await git(source.dir, [
    "diff",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--unified=5",
    source.base,
    source.head,
  ]);
  assert.ok(full.length > 200000);
  const first = await inspectionTool(source, "diff", {});
  assert.equal(first.truncated, true);
  assert.equal(first.nextOffset, 200000);
  assert.equal(first.total, full.length);
  assert.doesNotMatch(first.patch, /late change must remain reachable/);
  let assembled = first.patch;
  let offset = first.nextOffset;
  while (offset !== null) {
    const page = await inspectionTool(source, "diff", { offset });
    assert.equal(page.offset, offset);
    assert.equal(page.truncated, page.nextOffset !== null);
    assembled += page.patch;
    offset = page.nextOffset;
  }
  assert.equal(assembled, full);
  assert.match(assembled, /late change must remain reachable/);

  const changed = [];
  offset = 0;
  while (offset !== null) {
    const page = await inspectionTool(source, "list_files", {
      changed_only: true,
      offset,
      count: 2,
    });
    changed.push(...page.files);
    assert.equal(page.truncated, page.nextOffset !== null);
    offset = page.nextOffset;
  }
  assert.ok(changed.includes("deleted.txt"));
  assert.ok(changed.includes("z-later.txt"));
  assert.ok(!changed.includes("binary.dat"));
  assert.ok(!changed.includes("unrelated.txt"));
  const later = await inspectionTool(source, "diff", { path: "z-later.txt" });
  assert.equal(later.truncated, false);
  assert.match(later.patch, /late change must remain reachable/);
  const deleted = await inspectionTool(source, "diff", { path: "deleted.txt" });
  assert.match(deleted.patch, /deleted file mode/);
});

test("inspection pagination rejects invalid bounds and reports exhausted pages", async (t) => {
  const { source } = await fixture(t);
  for (const name of ["list_files", "diff"]) {
    for (const args of [
      { offset: -1 },
      { offset: 0.5 },
      { offset: Infinity },
      { offset: "0" },
      { count: 0 },
      { count: 200001 },
    ]) {
      await assert.rejects(inspectionTool(source, name, args), /offset must/);
    }
    const page = await inspectionTool(source, name, { offset: 1000000 });
    assert.equal(page.nextOffset, null);
    assert.equal(page.truncated, false);
    assert.equal((page.files ?? page.patch).length, 0);
  }
});

test("diffs larger than 16 MiB stream pages and bounded publication anchors", async (t) => {
  // A single enormous added line must not defeat either the aggregate-output
  // bound or a line-oriented reader. The later file must remain reachable.
  const { source } = await fixture(t, { largeDiff: 17 * 1024 * 1024 });
  const first = await inspectionTool(source, "diff", {});
  assert.equal(first.patch.length, 200000);
  assert.ok(first.total > 16 * 1024 * 1024);
  const last = await inspectionTool(source, "diff", {
    offset: first.total - 2000,
  });
  assert.equal(last.truncated, false);
  assert.match(last.patch, /late change must remain reachable/);
  const middle = await inspectionTool(source, "diff", {
    offset: 16 * 1024 * 1024,
    count: 100,
  });
  assert.equal(middle.patch, "x".repeat(100));
  const findings = [
    { path: "a-large.txt", line: 1 },
    { path: "z-later.txt", line: 1 },
    { path: "src/api.js", line: 1 }, // Context is not an added-line anchor.
    { path: "src/api.js", line: 2 },
    { path: "deleted.txt", line: 1 },
  ].map((finding, i) => ({
    ...finding,
    id: String(i),
    severity: "high",
    title: "Finding",
    body: "Details",
  }));
  const patch = await publicationPatch(source, findings);
  assert.ok(patch.length < 1000);
  assert.deepEqual(
    inlineComments({ summary: "Reviewed", findings }, patch).map(
      ({ path, line }) => ({ path, line }),
    ),
    [
      { path: "a-large.txt", line: 1 },
      { path: "z-later.txt", line: 1 },
      { path: "src/api.js", line: 2 },
    ],
  );
  const signal = AbortSignal.abort(new Error("Review superseded"));
  await assert.rejects(diff(source, undefined, { signal }), /superseded/);
  await assert.rejects(
    publicationPatch(source, findings, { signal }),
    /superseded/,
  );
});

test("tree and changed-path listings over 16 MiB stream pages and target guidance", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "crow-large-tree-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await git(dir, ["init", "--bare"]);
  const blob = async (body) =>
    (await git(dir, ["hash-object", "-w", "--stdin"], { input: body })).trim();
  const tree = async (entries) =>
    (
      await git(dir, ["mktree", "-z"], { input: entries.join("\0") + "\0" })
    ).trim();
  const trusted = await blob("Trusted target guidance.");
  const untrusted = await blob("Untrusted PR guidance.");
  const empty = await blob("");
  const leafNames = Array.from(
    { length: 1000 },
    (_, i) => `f${String(i).padStart(4, "0")}-${"x".repeat(180)}.txt`,
  );
  // Reuse one subtree 100 times. Git emits 100,000 paths without creating
  // that many objects or filesystem entries in this fixture.
  const subtree = await tree(
    leafNames.map((name) => `100644 blob ${empty}\t${name}`),
  );
  const directories = Array.from(
    { length: 100 },
    (_, i) => `dir${String(i).padStart(3, "0")}`,
  );
  const roots = directories.map((name) => `040000 tree ${subtree}\t${name}`);
  const base = await tree([
    `100644 blob ${trusted}\tAGENTS.md`,
    `100644 blob ${empty}\tdeleted.txt`,
  ]);
  const targetSha = await tree([...roots, `100644 blob ${trusted}\tAGENTS.md`]);
  const head = await tree([
    ...roots,
    `100644 blob ${untrusted}\tAGENTS.md`,
    `100644 blob ${empty}\tz-later.txt`,
  ]);
  const source = { dir, base, head, targetSha, target: "main" };
  assert.ok(
    100000 * (directories[0].length + 1 + leafNames[0].length + 1) >
      16 * 1024 * 1024,
  );
  const total = 100002;
  const first = await inspectionTool(source, "list_files", {});
  assert.equal(first.total, total);
  assert.equal(first.nextOffset, first.files.length);
  assert.equal(first.truncated, true);
  assert.ok(first.files.join("").length <= 200000);
  const later = await inspectionTool(source, "list_files", {
    offset: total - 2,
  });
  assert.deepEqual(later.files, [`dir099/${leafNames.at(-1)}`, "z-later.txt"]);
  assert.equal(later.nextOffset, null);
  const changed = await inspectionTool(source, "list_files", {
    changed_only: true,
    offset: total - 1,
  });
  assert.equal(changed.total, total + 1); // Includes the deleted base path.
  assert.deepEqual(changed.files, later.files);
  const filtered = await inspectionTool(source, "list_files", {
    prefix: "dir099/",
    offset: 998,
    count: 1,
  });
  assert.equal(filtered.total, 1000);
  assert.deepEqual(filtered.files, [`dir099/${leafNames[998]}`]);
  assert.equal(filtered.nextOffset, 999);
  const rules = await guidance(source);
  assert.deepEqual(rules.files, [
    { path: "AGENTS.md", body: "Trusted target guidance." },
  ]);
});

test("MCP advertises pagination and serializes completion metadata for reviewers", async (t) => {
  const { source, dir } = await fixture(t);
  const sourcePath = join(dir, "source.json");
  await writeFile(sourcePath, JSON.stringify(source));
  const response = await processRun(
    process.execPath,
    ["dist/bin/inspection-mcp.mjs", sourcePath],
    {
      input: [
        { jsonrpc: "2.0", id: 1, method: "tools/list" },
        {
          jsonrpc: "2.0",
          id: 2,
          method: "tools/call",
          params: {
            name: "list_files",
            arguments: { changed_only: true, count: 1 },
          },
        },
        {
          jsonrpc: "2.0",
          id: 3,
          method: "tools/call",
          params: { name: "diff", arguments: { count: 20 } },
        },
      ]
        .map((request) => JSON.stringify(request) + "\n")
        .join(""),
    },
  );
  const messages = response.stdout.trim().split("\n").map(JSON.parse);
  const definitions = messages[0].result.tools;
  for (const name of ["list_files", "diff"]) {
    const definition = definitions.find((tool) => tool.name === name);
    assert.equal(definition.inputSchema.properties.offset.minimum, 0);
    assert.match(definition.description, /nextOffset/);
  }
  assert.equal(
    definitions.find((tool) => tool.name === "list_files").inputSchema
      .properties.changed_only.type,
    "boolean",
  );
  const filesPage = JSON.parse(messages[1].result.content[0].text);
  assert.equal(filesPage.files.length, 1);
  assert.equal(filesPage.nextOffset, 1);
  assert.equal(filesPage.truncated, true);
  const diffPage = JSON.parse(messages[2].result.content[0].text);
  assert.equal(diffPage.patch.length, 20);
  assert.equal(diffPage.nextOffset, 20);
  assert.equal(diffPage.truncated, true);
});

test("diff does not invoke a configured external diff command", async (t) => {
  const { source, dir } = await fixture(t);
  const canary = join(dir, "external-diff-ran");
  const command = join(dir, "external-diff.sh");
  await writeFile(command, `#!/bin/sh\ntouch '${canary}'\n`, { mode: 0o700 });
  await git(source.dir, ["config", "diff.external", command]);
  assert.match((await diff(source)).patch, /const changed/);
  const { access } = await import("node:fs/promises");
  await assert.rejects(access(canary), (e) => e.code === "ENOENT");
});
