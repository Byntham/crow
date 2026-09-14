import { lstat, readdir, readFile, rm } from "node:fs/promises";
import { join, resolve } from "node:path";

export interface RetentionJob {
  id: string;
  state: string;
  updatedAt: number;
  session?: string | { id?: string } | null;
}

interface CleanupResult {
  removed: string[];
  warnings: string[];
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

const terminal = new Set(["completed", "superseded", "cancelled"]);
const identifier = /^[A-Za-z0-9_-]{1,100}$/;
const uuid = /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/i;
async function stat(path: string) {
  try {
    return await lstat(path);
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT")
      return null;
    throw error;
  }
}

// Parent directories must belong to this installation. Never walk a linked directory.
async function directory(path: string) {
  const info = await stat(path);
  if (!info) return false;
  if (info.isSymbolicLink() || !info.isDirectory())
    throw new Error(`Retention skipped non-directory or linked path: ${path}`);
  return true;
}

export async function cleanup(
  root: string,
  jobs: readonly RetentionJob[],
  days = 7,
  { now = Date.now() }: { now?: number } = {},
): Promise<CleanupResult> {
  if (!Number.isFinite(days) || days < 0 || !Number.isFinite(now) || now < 0)
    throw new Error(
      "Retention days and current time must be finite nonnegative values",
    );
  if (!Array.isArray(jobs))
    throw new Error("Retention requires a list of worker jobs");
  root = resolve(root);
  const result: CleanupResult = { removed: [], warnings: [] };
  if (!(await directory(root))) return result;
  const before = now - days * 86400000;
  const expired = jobs.filter(
    (job) =>
      identifier.test(job.id || "") &&
      terminal.has(job.state) &&
      Number.isFinite(job.updatedAt) &&
      job.updatedAt <= before,
  );
  const expiredIds = new Set(expired.map((job) => job.id));
  const sessionId = (job: RetentionJob) =>
    typeof job.session === "string" ? job.session : job.session?.id;
  // A session referenced by unfinished work must survive even if another record is terminal.
  const retainedSessions = new Set(
    jobs
      .filter((job) => !expiredIds.has(job.id))
      .map(sessionId)
      .filter((id): id is string => typeof id === "string" && id.length > 0),
  );
  const sessions = new Set(
    expired
      .map(sessionId)
      .filter(
        (id): id is string =>
          typeof id === "string" && uuid.test(id) && !retainedSessions.has(id),
      ),
  );
  async function childSessions(job: RetentionJob): Promise<string[]> {
    const found: string[] = [];
    for (const path of [
      join(root, "reviews"),
      join(root, "reviews", job.id),
      join(root, "reviews", job.id, "tasks"),
    ]) {
      if (!(await directory(path))) return found;
    }
    const tasks = join(root, "reviews", job.id, "tasks");
    for (const entry of await readdir(tasks, { withFileTypes: true })) {
      if (
        !entry.isFile() ||
        !entry.name.endsWith(".json") ||
        !identifier.test(entry.name.slice(0, -5))
      )
        continue;
      const file = join(tasks, entry.name);
      const info = await stat(file);
      if (!info?.isFile() || info.size > 1024 * 1024) continue;
      try {
        const task: unknown = JSON.parse(await readFile(file, "utf8"));
        if (
          typeof task === "object" &&
          task !== null &&
          "session" in task &&
          typeof task.session === "string" &&
          uuid.test(task.session)
        )
          found.push(task.session);
      } catch (error) {
        result.warnings.push(
          `Cannot read delegated session record ${entry.name}: ${errorMessage(error)}`,
        );
      }
    }
    return found;
  }
  // Session IDs are explicit worker records. Never infer ownership from rollout timestamps.
  for (const job of jobs.filter((job) => identifier.test(job.id || ""))) {
    try {
      for (const session of await childSessions(job)) {
        if (expiredIds.has(job.id)) sessions.add(session);
        else retainedSessions.add(session);
      }
    } catch (error) {
      result.warnings.push(errorMessage(error));
    }
  }
  for (const session of retainedSessions) sessions.delete(session);
  for (const job of expired) {
    let failed = false;
    for (const [folder, name] of [
      ["sources", job.id],
      ["reviews", job.id],
      ["reports", `${job.id}.json`],
      ["logs", `${job.id}.log`],
    ]) {
      try {
        const parent = join(root, folder);
        if (await directory(parent))
          await rm(join(parent, name), { recursive: true, force: true });
      } catch (error) {
        failed = true;
        result.warnings.push(errorMessage(error));
      }
    }
    if (!failed) result.removed.push(job.id);
  }
  async function walk(path: string): Promise<void> {
    if (!(await directory(path))) return;
    for (const entry of await readdir(path, { withFileTypes: true })) {
      // lstat/readdir do not follow links. rm of job roots unlinks links rather than their targets.
      if (entry.isSymbolicLink()) continue;
      const child = join(path, entry.name);
      if (entry.isDirectory()) await walk(child);
      else if (
        entry.isFile() &&
        entry.name.startsWith("rollout-") &&
        entry.name.endsWith(".jsonl")
      ) {
        const id = entry.name.slice(-42, -6);
        if (sessions.has(id)) await rm(child, { force: true });
      }
    }
  }
  if (sessions.size) {
    try {
      const codex = join(root, "codex");
      if (await directory(codex)) {
        await walk(join(codex, "sessions"));
        await walk(join(codex, "archived_sessions"));
      }
    } catch (error) {
      result.warnings.push(errorMessage(error));
    }
  }
  result.warnings = [...new Set(result.warnings)];
  return result;
}
