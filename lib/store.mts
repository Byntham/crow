import type {
  RecordMap,
  ReviewJob,
  RepositoryRecord,
  GitHubPullRequest,
  Comparison,
  RetrySettings,
  CrowEvent,
} from "./types.mjs";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { id, repoName } from "./util.mjs";
const terminal = new Set(["completed", "superseded", "cancelled"]);
interface PendingEvent extends CrowEvent {
  id: string;
  retries?: number;
  nextAt?: number;
}
export class Store {
  db: DatabaseSync;
  constructor(file: string) {
    mkdirSync(dirname(file), { recursive: true, mode: 0o700 });
    this.db = new DatabaseSync(file);
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,id));
      CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, received INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, value TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending');`);
  }
  close() {
    this.db.close();
  }
  tx<T>(fn: () => T): T {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const v = fn();
      this.db.exec("COMMIT");
      return v;
    } catch (e) {
      this.db.exec("ROLLBACK");
      throw e;
    }
  }
  // These private rows are written only through this typed API. Untrusted webhook,
  // provider and administrative data must be validated before reaching Store.
  get<K extends keyof RecordMap>(
    kind: K,
    key: string | number,
  ): RecordMap[K] | null {
    const r = this.db
      .prepare("SELECT value FROM records WHERE kind=? AND id=?")
      .get(kind, String(key));
    return r ? (JSON.parse(String(r.value)) as RecordMap[K]) : null;
  }
  all<K extends keyof RecordMap>(kind: K): RecordMap[K][] {
    return this.db
      .prepare("SELECT value FROM records WHERE kind=? ORDER BY rowid")
      .all(kind)
      .map((r) => JSON.parse(String(r.value)) as RecordMap[K]);
  }
  put<K extends keyof RecordMap>(
    kind: K,
    key: string | number,
    value: RecordMap[K],
  ): RecordMap[K] {
    this.db
      .prepare(
        "INSERT INTO records VALUES(?,?,?) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",
      )
      .run(kind, String(key), JSON.stringify(value));
    return value;
  }
  delete(kind: keyof RecordMap, key: string | number) {
    this.db
      .prepare("DELETE FROM records WHERE kind=? AND id=?")
      .run(kind, String(key));
  }
  acceptEvent(delivery: string, event: CrowEvent) {
    return this.tx(() => {
      if (this.db.prepare("SELECT 1 FROM receipts WHERE id=?").get(delivery))
        return false;
      this.db
        .prepare("INSERT INTO receipts VALUES(?,?)")
        .run(delivery, Date.now());
      this.db
        .prepare("INSERT INTO events(id,value) VALUES(?,?)")
        .run(delivery, JSON.stringify(event));
      return true;
    });
  }
  events(): PendingEvent[] {
    return this.db
      .prepare(
        "SELECT id,value FROM events WHERE state='pending' ORDER BY rowid",
      )
      .all()
      .map((x) => ({
        id: String(x.id),
        ...(JSON.parse(String(x.value)) as Omit<PendingEvent, "id">),
      }));
  }
  deferEvent(event: PendingEvent, nextAt: number) {
    const { id: key, ...value } = event;
    this.db
      .prepare("UPDATE events SET value=? WHERE id=?")
      .run(
        JSON.stringify({ ...value, retries: (event.retries || 0) + 1, nextAt }),
        key,
      );
  }
  eventDone(key: string) {
    this.db.prepare("DELETE FROM events WHERE id=?").run(key);
  }
  enroll(repo: RepositoryRecord) {
    repo.name = repoName(repo.name);
    return this.put("repos", repo.name, repo);
  }
  queue(
    repo: RepositoryRecord,
    pr: GitHubPullRequest,
    {
      manual = false,
      restart = false,
      held = false,
    }: { manual?: boolean; restart?: boolean; held?: boolean } = {},
  ) {
    return this.tx(() => {
      const key = `${repo.name}#${pr.number}`;
      const active = this.all("jobs").find(
        (j) => j.key === key && !terminal.has(j.state),
      );
      if (
        active &&
        active.head === pr.head.sha &&
        active.target === pr.base.ref &&
        !restart
      ) {
        if (manual && ["paused", "held"].includes(active.state)) {
          active.resumeEpoch = (active.resumeEpoch || 0) + 1;
          const usable =
            active.state === "held" || !active.startedAt || active.session;
          active.state = active.report || usable ? "queued" : "paused";
          active.reason =
            usable || active.report
              ? null
              : "Restart required: no saved provider session";
          active.retries = 0;
          active.nextAt = 0;
          this.put("jobs", active.id, active);
        }
        return active;
      }
      if (active) {
        active.state = "superseded";
        active.updatedAt = Date.now();
        this.put("jobs", active.id, active);
      }
      const job: ReviewJob = {
        id: id(),
        key,
        repo: repo.name,
        number: pr.number,
        head: pr.head.sha,
        target: pr.base.ref,
        worker: repo.worker,
        state: held ? "held" : "queued",
        manual,
        restart,
        priority: held ? 0 : manual ? 2 : 1,
        resumeEpoch: 0,
        createdAt: Date.now(),
        updatedAt: Date.now(),
        retries: 0,
        nextAt: 0,
        session: null,
        report: null,
      };
      return this.put("jobs", job.id, job);
    });
  }
  claim(
    worker: string,
    {
      excludeIds = [],
      excludeKeys = [],
    }: { excludeIds?: string[]; excludeKeys?: string[] } = {},
  ) {
    return this.tx(() => {
      const j = this.all("jobs")
        .sort((a, b) => (b.priority || 0) - (a.priority || 0))
        .find(
          (j) =>
            j.worker === worker &&
            !excludeIds.includes(j.id) &&
            !excludeKeys.includes(j.key) &&
            ["queued", "retrying"].includes(j.state) &&
            (j.nextAt || 0) <= Date.now(),
        );
      if (!j) return null;
      j.state = "reviewing";
      j.lease = id();
      j.startedAt ??= Date.now();
      j.updatedAt = Date.now();
      return this.put("jobs", j.id, j);
    });
  }
  updateJob(key: string, patch: Partial<ReviewJob>, lease?: string) {
    return this.tx(() => {
      const j = this.get("jobs", key);
      if (!j || (lease && (j.lease !== lease || j.state !== "reviewing")))
        throw new Error("Review ownership lost");
      return this.put("jobs", key, { ...j, ...patch, updatedAt: Date.now() });
    });
  }
  prune(days: number) {
    const before = Date.now() - days * 86400000;
    this.tx(() => {
      this.db
        .prepare(
          "DELETE FROM receipts WHERE received<? AND id NOT IN (SELECT id FROM events)",
        )
        .run(before);
      for (const job of this.all("jobs")) {
        if (!terminal.has(job.state) || job.updatedAt >= before) continue;
        delete job.report;
        delete job.patch;
        this.put("jobs", job.id, job);
      }
    });
  }
}
export function eligible(
  repo: Pick<RepositoryRecord, "policy" | "authors">,
  pr: GitHubPullRequest,
) {
  return (
    pr.state === "open" &&
    !pr.draft &&
    (repo.policy === "everyone" ||
      (repo.authors || []).some(
        (x) => x.toLowerCase() === pr.user.login.toLowerCase(),
      ))
  );
}
export function comparisonKey(c: Pick<Comparison, "head" | "target" | "base">) {
  return `${c.head}:${c.target}:${c.base}`;
}
export function retryDelay(
  retry: RetrySettings,
  attempt: number,
  providerWait = 0,
  random = Math.random,
) {
  const schedule = [5000, 15000, 30000, 60000, 120000, 300000];
  return (
    Math.max(
      providerWait,
      retry.mode === "progressive"
        ? schedule[Math.min(attempt - 1, schedule.length - 1)]
        : retry.delayMs,
    ) + Math.floor(random() * 500)
  );
}
