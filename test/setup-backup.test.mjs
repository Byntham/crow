import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
  stat,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createCipheriv, randomBytes, scryptSync } from "node:crypto";
import { defaults, save, load } from "../dist/lib/config.mjs";
import { Store } from "../dist/lib/store.mjs";
import { acquireLock } from "../dist/lib/util.mjs";
import { exportBackup, restoreBackup } from "../dist/lib/backup.mjs";

const passphrase = "a test-only backup passphrase";
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-backup-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "source");
  const config = defaults(source);
  config.operator = "test-user";
  config.app = {
    slug: "crow-test",
    id: 123,
    pem: "secret app key",
    webhookSecret: "secret webhook key",
  };
  await save(config, source);
  return { root, source, config, file: join(root, "backup.crow") };
}

test("encrypted export includes a consistent WAL snapshot and App credentials, excluding provider files", async (t) => {
  const { root, source, file } = await fixture(t);
  const store = new Store(join(source, "service.sqlite"));
  t.after(() => store.close());
  store.put("repos", "owner/repo", { name: "owner/repo" });
  await mkdir(join(source, "codex"));
  await writeFile(join(source, "codex/auth.json"), "provider-secret");
  const result = await exportBackup(source, file, passphrase);
  assert.deepEqual(result.files, ["config.json", "service.sqlite"]);
  const encrypted = await readFile(file);
  assert.equal(encrypted.includes(Buffer.from("secret app key")), false);
  assert.equal((await stat(file)).mode & 0o777, 0o600);
  const destination = join(root, "restored");
  await restoreBackup(destination, file, passphrase);
  assert.equal((await load(destination)).app.pem, "secret app key");
  assert.equal(
    (await load(destination)).worker.codexHome,
    join(destination, "codex"),
  );
  const restored = new Store(join(destination, "service.sqlite"));
  try {
    assert.deepEqual(restored.get("repos", "owner/repo"), {
      name: "owner/repo",
    });
  } finally {
    restored.close();
  }
  assert.equal(
    JSON.parse(
      await readFile(join(destination, "restore-pending.json"), "utf8"),
    ).providerSessionsRestored,
    false,
  );
  await assert.rejects(stat(join(destination, "codex/auth.json")), {
    code: "ENOENT",
  });
});

test("wrong passphrase and tampering leave destination configuration intact", async (t) => {
  const { root, source, file } = await fixture(t);
  await exportBackup(source, file, passphrase);
  const destination = join(root, "destination");
  const original = defaults(destination);
  original.operator = "original";
  await save(original, destination);
  await assert.rejects(
    restoreBackup(destination, file, "wrong passphrase"),
    /Cannot decrypt/,
  );
  const data = await readFile(file);
  data[data.length - 1] ^= 1;
  await writeFile(file, data);
  await assert.rejects(
    restoreBackup(destination, file, passphrase),
    /Cannot decrypt/,
  );
  assert.equal((await load(destination)).operator, "original");
});

test("restore refuses an active runtime and export refuses overwriting an archive", async (t) => {
  const { source, file } = await fixture(t);
  await exportBackup(source, file, passphrase);
  await assert.rejects(exportBackup(source, file, passphrase), {
    code: "EEXIST",
  });
  const release = await acquireLock(join(source, "runtime.lock"));
  try {
    await assert.rejects(restoreBackup(source, file, passphrase));
  } finally {
    await release();
  }
});

async function crafted(file, files) {
  const header = Buffer.from("CROWBACKUP\x01"),
    salt = randomBytes(16),
    nonce = randomBytes(12);
  const key = scryptSync(passphrase, salt, 32, {
    N: 32768,
    r: 8,
    p: 1,
    maxmem: 64 * 1024 * 1024,
  });
  const cipher = createCipheriv("aes-256-gcm", key, nonce);
  cipher.setAAD(header);
  const data = Buffer.concat([
    cipher.update(JSON.stringify({ version: 1, files })),
    cipher.final(),
  ]);
  await writeFile(
    file,
    Buffer.concat([header, salt, nonce, cipher.getAuthTag(), data]),
  );
}

test("restore validates exact archive names and configuration before replacing state", async (t) => {
  const { source, file, config } = await fixture(t);
  await crafted(file, {
    "config.json": Buffer.from(JSON.stringify(config)).toString("base64"),
    "../escaped": "eA==",
  });
  await assert.rejects(
    restoreBackup(source, file, passphrase),
    /Invalid Crow backup contents/,
  );
  await crafted(file, {
    "config.json": Buffer.from(
      JSON.stringify({ ...config, version: 999 }),
    ).toString("base64"),
  });
  await assert.rejects(
    restoreBackup(source, file, passphrase),
    /Unsupported configuration/,
  );
  assert.equal((await load(source)).version, 1);
});

test("invalid database does not replace the destination config", async (t) => {
  const { source, file, config } = await fixture(t);
  await crafted(file, {
    "config.json": Buffer.from(
      JSON.stringify({ ...config, operator: "replacement" }),
    ).toString("base64"),
    "service.sqlite": Buffer.from("not a database").toString("base64"),
  });
  await assert.rejects(restoreBackup(source, file, passphrase));
  assert.equal((await load(source)).operator, "test-user");
});
