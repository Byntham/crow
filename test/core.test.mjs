import test from 'node:test';
import assert from 'node:assert/strict';
import { createHmac } from 'node:crypto';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { changedLines, truncateDiff } from '../server/crow.mjs';
import { commandFor, parseProviderJson, runProcess, validate } from '../server/review-runner.mjs';
import { JobStore } from '../server/job-store.mjs';
import { verifySignature } from '../server/github.mjs';
import { createCrowServer } from '../server/crow.mjs';

test('changedLines returns added right-side lines by file', () => {
  const diff = ['diff --git a/src/a.ts b/src/a.ts', '+++ b/src/a.ts', '@@ -2,2 +2,3 @@', ' old', '+new one', '+new two', ' end'].join('\n');
  assert.deepEqual([...changedLines(diff).get('src/a.ts')], [3, 4]);
  const spaced = ['+++ b/docs/read me.md\t2026-01-01', '@@ -1 +1 @@', '+updated'].join('\n');
  assert.deepEqual([...changedLines(spaced).get('docs/read me.md')], [1]);
});

test('changedLines handles quoted UTF-8 paths, no-newline markers, and +++ content', () => {
  const diff = [
    'diff --git a/caf\\303\\251.txt b/caf\\303\\251.txt',
    '--- a/caf\\303\\251.txt',
    '+++ "b/caf\\303\\251.txt"',
    '@@ -1,2 +1,3 @@',
    ' same',
    '+first',
    '\\ No newline at end of file',
    '+++ literal source line',
    '+last'
  ].join('\n');
  assert.deepEqual([...changedLines(diff).get('café.txt')], [2, 3, 4]);
});

test('diff truncation respects the UTF-8 byte limit', () => {
  const clipped = truncateDiff('😀'.repeat(100), 80);
  assert.ok(Buffer.byteLength(clipped, 'utf8') <= 80);
  assert.match(clipped, /Diff truncated by Crow/);
});

test('webhook signatures are verified with timing-safe comparison', () => {
  const body = Buffer.from('{"ok":true}');
  const signature = `sha256=${createHmac('sha256', 'secret').update(body).digest('hex')}`;
  assert.equal(verifySignature(body, signature, 'secret'), true);
  assert.equal(verifySignature(body, signature.slice(0, -1) + '0', 'secret'), false);
  assert.equal(verifySignature(body, signature, 'wrong'), false);
});

test('provider JSON envelopes and findings are normalized', () => {
  const value = parseProviderJson(JSON.stringify({ result: JSON.stringify({ summary: 'ok', findings: [{ severity: 'critical', title: 'Bug', body: 'Details', recommendation: 'Fix', path: 'a.ts', line: 3, confidence: 0.9 }] }) }), 'claude');
  assert.equal(validate(value).findings.length, 1);
  assert.equal(parseProviderJson('{"summary":"ok","findings":[]}').summary, 'ok');
  const structured = parseProviderJson(JSON.stringify({ type: 'result', result: 'human-readable text', structured_output: { summary: 'structured', findings: [] } }), 'claude');
  assert.equal(validate(structured).summary, 'structured');
});

test('provider commands stay non-interactive and read-only', () => {
  const codex = commandFor('codex', '/tmp/schema.json');
  assert.ok(codex.args.includes('--sandbox') && codex.args.includes('read-only'));
  assert.ok(codex.args.includes('approval_policy="never"'));
  const claude = commandFor('claude', '/tmp/schema.json');
  assert.ok(claude.args.includes('--permission-mode') && claude.args.includes('dontAsk'));
  assert.ok(claude.args.includes('--tools') && claude.args.includes('Read'));
  assert.ok(claude.args.includes('--restricted') && claude.args.includes('--safe-mode') && claude.args.includes('--strict-mcp-config'));
  assert.equal(claude.args.includes('--bare'), false);
  assert.ok(codex.args.includes('--ignore-user-config') && codex.args.includes('project_doc_max_bytes=0'));
});

test('provider output redacts review-time secrets', async () => {
  const result = await runProcess(process.execPath, ['-e', "process.stdout.write('installation-secret-value')"], {
    cwd: process.cwd(),
    redactionSecrets: ['installation-secret-value']
  });
  assert.equal(result.stdout, '[redacted]');
});

test('job store persists completed jobs and avoids duplicate enqueue', async () => {
  const root = await mkdtemp(join(tmpdir(), 'crow-test-'));
  const file = join(root, 'state.json');
  try {
    const job = { installationId: 1, repo: 'acme/app', number: 2, sha: 'a'.repeat(40) };
    const store = await new JobStore(file).load();
    assert.equal(await store.enqueue(job), true);
    assert.equal(await store.enqueue(job), false);
    const taken = await store.take();
    assert.deepEqual({ installationId: taken.installationId, repo: taken.repo, number: taken.number, sha: taken.sha }, job);
    await store.complete(job, 2);
    const restored = await new JobStore(file).load();
    assert.equal(await restored.enqueue(job), false);
    assert.equal(restored.history[0].findings, 2);

    const second = { ...job, sha: 'b'.repeat(40) };
    await restored.enqueue(second);
    await restored.take();
    const recovered = await new JobStore(file).load();
    assert.equal(recovered.pendingCount, 1);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('job store coalesces updates and durably defers work when the queue is full', async () => {
  const root = await mkdtemp(join(tmpdir(), 'crow-deferred-'));
  try {
    const store = await new JobStore(join(root, 'state.json')).load();
    const first = { installationId: 1, repo: 'acme/app', number: 3, sha: 'a'.repeat(40) };
    const update = { ...first, sha: 'b'.repeat(40) };
    const other = { installationId: 1, repo: 'acme/other', number: 4, sha: 'c'.repeat(40) };
    assert.equal(await store.enqueue(first, 1), true);
    assert.equal(await store.enqueue(update, 1), true);
    assert.equal(store.state.pending[0].sha, update.sha);
    assert.equal(await store.enqueue(other, 1), true);
    assert.equal(store.deferredCount, 1);
    const taken = await store.take();
    assert.equal(taken.sha, update.sha);
    await store.complete(taken);
    await store.promote(1);
    assert.equal(store.state.pending[0].sha, other.sha);

    const reopenStore = await new JobStore(join(root, 'reopen-state.json')).load();
    const reopenable = { installationId: 1, repo: 'acme/reopen', number: 5, sha: 'd'.repeat(40) };
    await reopenStore.enqueue(reopenable, 1);
    const inFlight = await reopenStore.take();
    await reopenStore.release(inFlight);
    assert.equal(await reopenStore.enqueue(reopenable, 1), true);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('job store does not retry an old commit after a newer update is queued', async () => {
  const root = await mkdtemp(join(tmpdir(), 'crow-supersede-'));
  try {
    const store = await new JobStore(join(root, 'state.json')).load();
    const oldJob = { installationId: 1, repo: 'acme/app', number: 6, sha: 'a'.repeat(40) };
    const newJob = { ...oldJob, sha: 'b'.repeat(40) };
    await store.enqueue(oldJob);
    const taken = await store.take();
    await store.enqueue(newJob);
    await store.retry(taken, new Error('temporary failure'));
    assert.equal(store.pendingCount, 1);
    assert.equal(store.state.pending[0].sha, newJob.sha);
    assert.equal(store.state.pending.filter(item => item.sha === oldJob.sha).length, 0);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('pull request webhook is accepted and handed to the worker', async () => {
  const root = await mkdtemp(join(tmpdir(), 'crow-webhook-'));
  const store = new JobStore(join(root, 'state.json'));
  await store.load();
  let received;
  const server = createCrowServer({
    store,
    webhookSecret: 'secret',
    reviewer: async (job) => { received = job; return { findings: 1 }; }
  });
  await new Promise(resolve => server.listen(0, resolve));
  try {
    const body = JSON.stringify({ action: 'synchronize', installation: { id: 9 }, repository: { full_name: 'acme/app' }, number: 7, pull_request: { head: { sha: 'c'.repeat(40) } } });
    const signature = `sha256=${createHmac('sha256', 'secret').update(body).digest('hex')}`;
    const response = await fetch(`http://127.0.0.1:${server.address().port}/webhooks/github`, { method: 'POST', headers: { 'x-github-event': 'pull_request', 'x-hub-signature-256': signature }, body });
    assert.equal(response.status, 202);
    for (let i = 0; i < 20 && !received; i += 1) await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal(received.repo, 'acme/app');
    assert.equal(received.number, 7);
  } finally { server.close(); await rm(root, { recursive: true, force: true }); }
});

test('duplicate webhook is acknowledged without starting a second job', async () => {
  const root = await mkdtemp(join(tmpdir(), 'crow-duplicate-'));
  const store = await new JobStore(join(root, 'state.json')).load();
  let calls = 0;
  let resolveFirst;
  const firstCall = new Promise(resolve => { resolveFirst = resolve; });
  const server = createCrowServer({ store, webhookSecret: 'secret', reviewer: async () => { calls += 1; resolveFirst?.(); await new Promise(resolve => setTimeout(resolve, 30)); return { findings: 0 }; } });
  await new Promise(resolve => server.listen(0, resolve));
  try {
    const payload = { action: 'opened', installation: { id: 9 }, repository: { full_name: 'acme/app' }, number: 8, pull_request: { head: { sha: 'd'.repeat(40) } } };
    const body = JSON.stringify(payload);
    const headers = { 'x-github-event': 'pull_request', 'x-hub-signature-256': `sha256=${createHmac('sha256', 'secret').update(body).digest('hex')}` };
    const url = `http://127.0.0.1:${server.address().port}/webhooks/github`;
    const first = await fetch(url, { method: 'POST', headers, body });
    const second = await fetch(url, { method: 'POST', headers, body });
    assert.equal(first.status, 202);
    assert.equal(second.status, 202);
    assert.equal(JSON.parse(await second.text()).accepted, false);
    await firstCall;
    assert.equal(calls, 1);
  } finally { server.close(); await rm(root, { recursive: true, force: true }); }
});
