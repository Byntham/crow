import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';

/** Small durable queue/state file for a single Crow instance. */
export class JobStore {
  constructor(file = process.env.CROW_STATE_FILE || join(process.cwd(), '.crow-data', 'state.json')) {
    this.file = file;
    this.state = { pending: [], inFlight: [], deferred: {}, completed: {}, attempts: {}, history: [] };
    this.writeChain = Promise.resolve();
  }

  async load() {
    try {
      const parsedValue = JSON.parse(await readFile(this.file, 'utf8'));
      const parsed = parsedValue && typeof parsedValue === 'object' ? parsedValue : {};
      this.state = { ...this.state, ...parsed, inFlight: parsed.inFlight || [], deferred: parsed.deferred || {} };
      if (!Array.isArray(this.state.pending)) this.state.pending = [];
      if (!Array.isArray(this.state.inFlight)) this.state.inFlight = [];
      if (!this.state.deferred || typeof this.state.deferred !== 'object' || Array.isArray(this.state.deferred)) this.state.deferred = {};
      if (!this.state.completed || typeof this.state.completed !== 'object' || Array.isArray(this.state.completed)) this.state.completed = {};
      if (!this.state.attempts || typeof this.state.attempts !== 'object' || Array.isArray(this.state.attempts)) this.state.attempts = {};
      if (!Array.isArray(this.state.history)) this.state.history = [];
      // A process can stop after taking a job but before it posts the review.
      // Put those jobs back in front of the queue on the next start.
      const recovered = this.state.inFlight.filter(item => !this.state.completed[this.key(item)]);
      const pending = [...recovered, ...(this.state.pending || [])];
      const keys = new Set();
      this.state.pending = pending.filter(item => { const key = this.key(item); if (keys.has(key) || this.state.completed[key]) return false; keys.add(key); return true; });
      for (const [prKey, item] of Object.entries(this.state.deferred)) {
        const key = this.key(item);
        if (keys.has(key) || this.state.completed[key]) delete this.state.deferred[prKey];
        else keys.add(key);
      }
      this.state.inFlight = [];
      this.prune();
    } catch (error) {
      if (error.code !== 'ENOENT') throw new Error(`Cannot read Crow state: ${error.message}`);
    }
    return this;
  }

  prune(now = Date.now()) {
    const ttl = 7 * 24 * 60 * 60 * 1000;
    for (const [key, timestamp] of Object.entries(this.state.completed)) if (now - timestamp > ttl) delete this.state.completed[key];
    this.state.history = (this.state.history || []).slice(-100);
    for (const [prKey, item] of Object.entries(this.state.deferred)) {
      if (!item || (item.queuedAt && now - item.queuedAt > ttl)) delete this.state.deferred[prKey];
    }
  }

  async persist() {
    this.prune();
    const payload = JSON.stringify(this.state, null, 2);
    this.writeChain = this.writeChain.then(async () => {
      await mkdir(dirname(this.file), { recursive: true, mode: 0o700 });
      const temp = `${this.file}.${process.pid}.tmp`;
      await writeFile(temp, payload, { mode: 0o600 });
      await rename(temp, this.file);
    });
    return this.writeChain;
  }

  key(job) { return `${job.repo}#${job.number}@${job.sha}${job.baseSha ? `:${job.baseSha}` : ''}`; }
  prKey(job) { return `${job.repo}#${job.number}`; }
  has(job) {
    const key = this.key(job);
    return Boolean(this.state.completed[key] || this.state.pending.some(item => this.key(item) === key) || this.state.inFlight.some(item => this.key(item) === key) || Object.values(this.state.deferred).some(item => this.key(item) === key));
  }

  async enqueue(job, maxQueue = Infinity) {
    const key = this.key(job);
    if (this.has(job)) return false;
    // A PR can generate several synchronize events while a review is running.
    // Keep only its newest pending commit; reviewing an older one is never
    // useful and would consume the queue.
    const pendingIndex = this.state.pending.findIndex(item => this.prKey(item) === this.prKey(job));
    if (pendingIndex >= 0) {
      this.state.pending[pendingIndex] = { ...job, queuedAt: this.state.pending[pendingIndex].queuedAt || Date.now() };
      await this.persist();
      return true;
    }
    const deferredKey = this.prKey(job);
    if (this.state.deferred[deferredKey]) {
      this.state.deferred[deferredKey] = { ...job, queuedAt: this.state.deferred[deferredKey].queuedAt || Date.now() };
      await this.persist();
      return true;
    }
    if (this.state.pending.length >= maxQueue) return this.defer(job);
    this.state.pending.push({ ...job, queuedAt: Date.now() });
    await this.persist();
    return true;
  }

  async defer(job) {
    const key = this.key(job);
    if (this.has(job)) return false;
    this.state.deferred[this.prKey(job)] = { ...job, queuedAt: Date.now() };
    await this.persist();
    return true;
  }

  async promote(maxQueue = Infinity) {
    let changed = false;
    while (this.state.pending.length < maxQueue) {
      const entries = Object.entries(this.state.deferred);
      if (!entries.length) break;
      entries.sort(([, a], [, b]) => (a.queuedAt || 0) - (b.queuedAt || 0));
      const [prKey, job] = entries[0];
      delete this.state.deferred[prKey];
      if (this.state.completed[this.key(job)] || this.state.inFlight.some(item => this.key(item) === this.key(job))) {
        changed = true;
        continue;
      }
      const pendingIndex = this.state.pending.findIndex(item => this.prKey(item) === this.prKey(job));
      if (pendingIndex >= 0) this.state.pending[pendingIndex] = job;
      else this.state.pending.push(job);
      changed = true;
    }
    if (changed) await this.persist();
    return changed;
  }

  async take(now = Date.now()) {
    const index = this.state.pending.findIndex(item => !item.retryAt || item.retryAt <= now);
    const job = index === -1 ? undefined : this.state.pending.splice(index, 1)[0];
    if (job) { this.state.inFlight.push(job); await this.persist(); }
    return job;
  }

  get nextRetryAt() {
    return [...this.state.pending, ...Object.values(this.state.deferred)].reduce((soonest, item) => item.retryAt && item.retryAt < soonest ? item.retryAt : soonest, Infinity);
  }

  async complete(job, findings = 0) {
    this.state.inFlight = this.state.inFlight.filter(item => this.key(item) !== this.key(job));
    this.state.completed[this.key(job)] = Date.now();
    this.state.history.push({ ...job, findings, completedAt: Date.now() });
    delete this.state.attempts[this.key(job)];
    await this.persist();
  }

  // Release a job without marking its commit as reviewed. This is used when
  // a PR is temporarily closed; a later `reopened` webhook must be able to
  // enqueue the same head commit.
  async release(job) {
    const key = this.key(job);
    this.state.inFlight = this.state.inFlight.filter(item => this.key(item) !== key);
    delete this.state.attempts[key];
    await this.persist();
  }

  async retry(job, error, maxAttempts = 3, maxQueue = Infinity) {
    const key = this.key(job);
    const errorMessage = String(error?.message || error || 'review failed').slice(0, 2_000);
    this.state.inFlight = this.state.inFlight.filter(item => this.key(item) !== key);
    const attempts = (this.state.attempts[key] || 0) + 1;
    // If a newer synchronize event for this PR is already queued or running,
    // retrying the failed commit would waste provider time and can publish a
    // stale review after the newer one. Let that newer job supersede it.
    const prKey = this.prKey(job);
    const superseded = [
      ...this.state.pending,
      ...Object.values(this.state.deferred),
      ...this.state.inFlight
    ].some(item => this.prKey(item) === prKey && this.key(item) !== key);
    if (superseded) {
      delete this.state.attempts[key];
      this.state.history.push({ ...job, error: errorMessage, supersededAt: Date.now() });
      await this.persist();
      return attempts;
    }
    if (attempts >= maxAttempts) {
      this.state.history.push({ ...job, error: errorMessage, failedAt: Date.now() });
      delete this.state.attempts[key];
    } else {
      this.state.attempts[key] = attempts;
      const retryJob = { ...job, queuedAt: Date.now(), retryAt: Date.now() + attempts * 5000 };
      if (this.state.pending.length >= maxQueue) this.state.deferred[this.prKey(job)] = retryJob;
      else this.state.pending.push(retryJob);
    }
    await this.persist();
    return attempts;
  }

  get pendingCount() { return this.state.pending.length; }
  get deferredCount() { return Object.keys(this.state.deferred).length; }
  get queuedCount() { return this.pendingCount + this.deferredCount; }
  get history() { return this.state.history || []; }
}
