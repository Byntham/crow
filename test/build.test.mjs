import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  readdir,
  rm,
  access,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { build } from "../scripts/build.mjs";

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-typescript-build-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "bin"));
  await mkdir(join(root, "lib"));
  await writeFile(
    join(root, "lib/value.mts"),
    "export interface Value { count: number }\nexport const value: Value = { count: 7 };\n",
  );
  await writeFile(
    join(root, "bin/crow.mts"),
    '#!/usr/bin/env node\nimport { value } from "../lib/value.mjs";\nconsole.log(value.count);\n',
  );
  return root;
}

test("dependency-free build emits runnable JavaScript with the installed module layout", async (t) => {
  const root = await fixture(t);
  const output = join(root, "release with spaces");
  const result = await build({ sourceRoot: root, outDir: output });
  assert.equal(result.modules, 2);
  assert.deepEqual((await readdir(output)).sort(), ["bin", "lib"]);
  const cli = spawnSync(process.execPath, [join(output, "bin/crow.mjs")], {
    encoding: "utf8",
  });
  assert.equal(cli.status, 0, cli.stderr);
  assert.equal(cli.stdout.trim(), "7");
  assert.doesNotMatch(
    await readFile(join(output, "lib/value.mjs"), "utf8"),
    /interface Value|: Value/,
  );
  await assert.rejects(access(join(output, "lib/value.mts")));
});

test("failed TypeScript stripping preserves the previous complete build and successful rebuild removes stale modules", async (t) => {
  const root = await fixture(t);
  await build({ sourceRoot: root });
  const previous = await readFile(join(root, "dist/lib/value.mjs"), "utf8");
  await writeFile(
    join(root, "lib/value.mts"),
    "export enum Value { First, Second }\n",
  );
  await assert.rejects(build({ sourceRoot: root }), /enum|strip-only/i);
  assert.equal(
    await readFile(join(root, "dist/lib/value.mjs"), "utf8"),
    previous,
  );
  assert.equal(
    (await readdir(root)).some((entry) => entry.startsWith(".crow-build-")),
    false,
  );
  await writeFile(
    join(root, "lib/value.mts"),
    "export const value: { count: number } = { count: 9 };\n",
  );
  await writeFile(
    join(root, "dist/lib/stale.mjs"),
    "throw new Error('stale');\n",
  );
  await build({ sourceRoot: root });
  await assert.rejects(access(join(root, "dist/lib/stale.mjs")));
});

test("build refuses output paths that would replace source directories", async (t) => {
  const root = await fixture(t);
  for (const output of [root, join(root, "lib"), join(root, "lib/generated")]) {
    await assert.rejects(
      build({ sourceRoot: root, outDir: output }),
      /separate from Crow source/,
    );
  }
  assert.match(
    await readFile(join(root, "lib/value.mts"), "utf8"),
    /interface Value/,
  );
});
