import {
  randomBytes,
  scrypt as derive,
  createCipheriv,
  createDecipheriv,
} from "node:crypto";
import {
  readFile,
  writeFile,
  open,
  mkdtemp,
  mkdir,
  rm,
  rename,
  stat,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { validateConfig } from "./config.mjs";
import { acquireLock } from "./util.mjs";

const HEADER = Buffer.from("CROWBACKUP\x01");
const MAX_BYTES = 128 * 1024 * 1024;
const names = ["config.json", "service.sqlite"];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function exists(file: string) {
  try {
    await stat(file);
    return true;
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT")
      return false;
    throw error;
  }
}
async function keyFor(passphrase: string, salt: Buffer): Promise<Buffer> {
  if (typeof passphrase !== "string" || !passphrase.length)
    throw new Error("A backup passphrase is required");
  return new Promise((resolve, reject) => {
    derive(
      passphrase,
      salt,
      32,
      {
        N: 32768,
        r: 8,
        p: 1,
        maxmem: 64 * 1024 * 1024,
      },
      (error, key) => {
        if (error) reject(error);
        else resolve(key);
      },
    );
  });
}
async function readLimited(file: string) {
  if ((await stat(file)).size > MAX_BYTES)
    throw new Error("Backup exceeds the 128 MiB limit");
  const bytes = await readFile(file);
  if (bytes.length > MAX_BYTES)
    throw new Error("Backup exceeds the 128 MiB limit");
  return bytes;
}
function validateDatabase(file: string) {
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    if (db.prepare("PRAGMA integrity_check").get()?.integrity_check !== "ok")
      throw new Error("Backup database integrity check failed");
    // Reject unrelated SQLite files before replacing Crow's state.
    for (const table of ["records", "receipts", "events"]) {
      if (
        !db
          .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")
          .get(table)
      )
        throw new Error(`Backup database is missing ${table}`);
    }
  } finally {
    db.close();
  }
}

/** Export configuration, App credentials and service state; provider homes are excluded. */
export async function exportBackup(
  root: string,
  file: string,
  passphrase: string,
) {
  root = resolve(root);
  file = resolve(file);
  if (names.some((name) => file === join(root, name)))
    throw new Error("Backup output cannot replace live Crow state");
  const salt = randomBytes(16),
    nonce = randomBytes(12),
    key = await keyFor(passphrase, salt);
  const temp = await mkdtemp(join(root, ".backup-"));
  try {
    const config = await readLimited(join(root, "config.json"));
    validateConfig(JSON.parse(config.toString("utf8")));
    const files: Record<string, string> = {
      "config.json": config.toString("base64"),
    };
    if (await exists(join(root, "service.sqlite"))) {
      const snapshot = join(temp, "service.sqlite");
      const db = new DatabaseSync(join(root, "service.sqlite"), {
        readOnly: true,
      });
      try {
        db.exec("PRAGMA busy_timeout=5000");
        db.prepare("VACUUM INTO ?").run(snapshot);
      } finally {
        db.close();
      }
      validateDatabase(snapshot);
      files["service.sqlite"] = (await readLimited(snapshot)).toString(
        "base64",
      );
    }
    const payload = Buffer.from(
      JSON.stringify({
        version: 1,
        createdAt: new Date().toISOString(),
        files,
      }),
    );
    if (payload.length > MAX_BYTES - 1024)
      throw new Error("Backup exceeds the 128 MiB limit");
    const cipher = createCipheriv("aes-256-gcm", key, nonce);
    cipher.setAAD(HEADER);
    const encrypted = Buffer.concat([cipher.update(payload), cipher.final()]);
    await mkdir(dirname(file), { recursive: true, mode: 0o700 });
    // Exclusive create protects an earlier backup from accidental replacement.
    const output = await open(file, "wx", 0o600);
    try {
      await output.writeFile(
        Buffer.concat([HEADER, salt, nonce, cipher.getAuthTag(), encrypted]),
      );
      await output.sync();
    } catch (error) {
      await rm(file, { force: true });
      throw error;
    } finally {
      await output.close();
    }
    return { file, files: Object.keys(files) };
  } finally {
    key.fill(0);
    await rm(temp, { recursive: true, force: true });
  }
}

/** Restore while the runtime is stopped. Failed replacements roll back the prior files. */
export async function restoreBackup(
  root: string,
  file: string,
  passphrase: string,
) {
  root = resolve(root);
  const bytes = await readLimited(file);
  const offset = HEADER.length;
  if (bytes.length < offset + 45 || !bytes.subarray(0, offset).equals(HEADER))
    throw new Error("Unsupported Crow backup");
  const key = await keyFor(passphrase, bytes.subarray(offset, offset + 16));
  let archive: unknown;
  try {
    const decipher = createDecipheriv(
      "aes-256-gcm",
      key,
      bytes.subarray(offset + 16, offset + 28),
    );
    decipher.setAAD(HEADER);
    decipher.setAuthTag(bytes.subarray(offset + 28, offset + 44));
    archive = JSON.parse(
      Buffer.concat([
        decipher.update(bytes.subarray(offset + 44)),
        decipher.final(),
      ]).toString("utf8"),
    );
  } catch {
    throw new Error(
      "Cannot decrypt backup: incorrect passphrase or damaged file",
    );
  } finally {
    key.fill(0);
  }
  if (
    !isRecord(archive) ||
    archive.version !== 1 ||
    !isRecord(archive.files) ||
    !Object.hasOwn(archive.files, "config.json") ||
    Object.keys(archive.files).some((name) => !names.includes(name))
  )
    throw new Error("Invalid Crow backup contents");
  const content: Record<string, Buffer> = {};
  for (const [name, value] of Object.entries(archive.files)) {
    if (
      typeof value !== "string" ||
      !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(
        value,
      )
    )
      throw new Error("Invalid backup file encoding");
    content[name] = Buffer.from(value, "base64");
  }
  const config = validateConfig(
    JSON.parse(content["config.json"].toString("utf8")),
  );
  // Authentication is established on the destination; no source provider home is reused.
  config.worker.codexHome = join(root, "codex");
  content["config.json"] = Buffer.from(JSON.stringify(config, null, 2) + "\n");
  await mkdir(root, { recursive: true, mode: 0o700 });
  const release = await acquireLock(join(root, "runtime.lock"));
  let temp: string | undefined,
    preserveTemp = false;
  try {
    temp = await mkdtemp(join(root, ".restore-"));
    for (const [name, value] of Object.entries(content))
      await writeFile(join(temp, name), value, { mode: 0o600 });
    if (content["service.sqlite"])
      validateDatabase(join(temp, "service.sqlite"));
    await writeFile(
      join(temp, "restore-pending.json"),
      JSON.stringify({
        version: 1,
        restoredAt: new Date().toISOString(),
        backupCreatedAt: archive.createdAt,
        providerSessionsRestored: false,
      }) + "\n",
      { mode: 0o600 },
    );
    const targets = [
      ...names,
      "service.sqlite-wal",
      "service.sqlite-shm",
      "restore-pending.json",
    ];
    const moved: string[] = [],
      installed: string[] = [];
    try {
      for (const name of targets) {
        if (await exists(join(root, name))) {
          await rename(join(root, name), join(temp, `${name}.previous`));
          moved.push(name);
        }
      }
      for (const name of [...Object.keys(content), "restore-pending.json"]) {
        await rename(join(temp, name), join(root, name));
        installed.push(name);
      }
    } catch (error) {
      try {
        for (const name of installed.reverse())
          await rm(join(root, name), { force: true });
        for (const name of moved.reverse())
          await rename(join(temp, `${name}.previous`), join(root, name));
      } catch (rollbackError) {
        preserveTemp = true;
        throw new AggregateError(
          [error, rollbackError],
          `Restore failed; previous files were retained in ${temp}`,
        );
      }
      throw error;
    }
    return { root, files: Object.keys(content), requiresProviderLogin: true };
  } finally {
    try {
      if (temp && !preserveTemp)
        await rm(temp, { recursive: true, force: true });
    } finally {
      await release();
    }
  }
}
