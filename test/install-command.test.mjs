import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  writeFile,
  readFile,
  readlink,
  symlink,
  rm,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { installDownloaded } from "../dist/lib/install-command.mjs";
import { acquireLock } from "../dist/lib/util.mjs";

async function fixture(t) {
  const dir = await mkdtemp(join(tmpdir(), "crow-install-command-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const root = join(dir, "state"),
    executable = join(dir, "crow"),
    binDir = join(dir, "bin");
  await writeFile(executable, "original binary");
  return {
    root,
    executable,
    options: { executable, binDir, version: "0.2.0" },
  };
}

test("initial and repeated installation preserve configuration and active runtime records", async (t) => {
  const { root, options } = await fixture(t);
  const installed = await installDownloaded(root, options);
  const current = await readlink(join(root, "current"));
  await writeFile(join(root, "config.json"), "operator config");
  const release = await acquireLock(join(root, "runtime.lock"));
  try {
    const lock = await readFile(join(root, "runtime.lock"));
    assert.equal(await installDownloaded(root, options), installed);
    assert.equal(await readlink(join(root, "current")), current);
    assert.equal(
      await readFile(join(root, "config.json"), "utf8"),
      "operator config",
    );
    assert.deepEqual(await readFile(join(root, "runtime.lock")), lock);
  } finally {
    await release();
  }
});

test("a different binary cannot bypass update even when reporting the same version", async (t) => {
  const { root, executable, options } = await fixture(t);
  const installed = await installDownloaded(root, options);
  const current = await readlink(join(root, "current"));
  await writeFile(executable, "changed binary");
  await assert.rejects(installDownloaded(root, options), /crow update/);
  assert.equal(await readFile(installed, "utf8"), "original binary");
  assert.equal(await readlink(join(root, "current")), current);
  await assert.rejects(readFile(join(root, "install.lock")), {
    code: "ENOENT",
  });
});

test("a source installation or broken current link is not replaced by bootstrap", async (t) => {
  const { root, options } = await fixture(t);
  await mkdir(root);
  await symlink(join(root, "source-release"), join(root, "current"));
  await assert.rejects(
    installDownloaded(root, options),
    /different Crow installation/,
  );
  assert.equal(
    await readlink(join(root, "current")),
    join(root, "source-release"),
  );
});

test("concurrent installers cannot change the same installation", async (t) => {
  const { root, options } = await fixture(t);
  const release = await acquireLock(join(root, "install.lock"));
  try {
    await assert.rejects(installDownloaded(root, options), /lock/);
    await assert.rejects(readlink(join(root, "current")), { code: "ENOENT" });
  } finally {
    await release();
  }
});
