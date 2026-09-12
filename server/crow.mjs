import { createServer } from 'node:http';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { getPullRequest, installationToken, listIssueComments, listReviews, postIssueComment, postReview, GitHubError, verifySignature } from './github.mjs';
import { JobStore } from './job-store.mjs';
import { runReview } from './review-runner.mjs';

const exec = promisify(execFile);
const positiveNumber = (value, fallback) => { const number = Number(value); return Number.isFinite(number) && number > 0 ? number : fallback; };
const boundedPositive = (value, fallback, maximum) => Math.min(maximum, positiveNumber(value, fallback));
const MAX_WEBHOOK_BYTES = boundedPositive(process.env.CROW_MAX_WEBHOOK_BYTES, 2 * 1024 * 1024, 16 * 1024 * 1024);
const MAX_DIFF_BYTES = boundedPositive(process.env.CROW_MAX_DIFF_BYTES, 4 * 1024 * 1024, 32 * 1024 * 1024);
const REVIEW_TIMEOUT_MS = boundedPositive(process.env.CROW_REVIEW_TIMEOUT_MS, 180_000, 15 * 60 * 1000);
const MAX_PROVIDER_OUTPUT_BYTES = boundedPositive(process.env.CROW_MAX_PROVIDER_OUTPUT_BYTES, 8 * 1024 * 1024, 32 * 1024 * 1024);
const MAX_QUEUE = Math.floor(boundedPositive(process.env.CROW_MAX_QUEUE, 100, 10_000));
const port = Math.floor(Math.min(65_535, positiveNumber(process.env.PORT, 8787)));
// Keep the webhook listener private by default; a TLS reverse proxy should be
// the public edge. Operators who intentionally expose Crow directly can set
// CROW_BIND_HOST (for example, 0.0.0.0) and enforce HTTPS/firewall rules.
const bindHost = process.env.CROW_BIND_HOST?.trim() || '127.0.0.1';

function configuredSecret() {
  if (process.env.GITHUB_WEBHOOK_SECRET?.trim()) return process.env.GITHUB_WEBHOOK_SECRET.trim();
  if (process.env.GITHUB_WEBHOOK_SECRET_FILE) {
    try { return readFileSync(process.env.GITHUB_WEBHOOK_SECRET_FILE, 'utf8').trim(); } catch { return undefined; }
  }
  return undefined;
}

function configuredPrivateKey() {
  if (process.env.GITHUB_PRIVATE_KEY?.trim()) return true;
  if (!process.env.GITHUB_PRIVATE_KEY_FILE) return false;
  try { return Boolean(readFileSync(process.env.GITHUB_PRIVATE_KEY_FILE, 'utf8').trim()); } catch { return false; }
}

function configuredPrivateKeyValue() {
  if (process.env.GITHUB_PRIVATE_KEY?.trim()) return process.env.GITHUB_PRIVATE_KEY;
  if (!process.env.GITHUB_PRIVATE_KEY_FILE) return undefined;
  try { return readFileSync(process.env.GITHUB_PRIVATE_KEY_FILE, 'utf8'); } catch { return undefined; }
}

const repoPattern = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const shaPattern = /^[0-9a-f]{40}$/i;

function validRepo(value) {
  return typeof value === 'string' && repoPattern.test(value) && value.split('/').every(part => part !== '.' && part !== '..');
}

const gitBaseEnv = ['PATH', 'HOME', 'USER', 'LANG', 'LC_ALL', 'TMPDIR'];
async function git(args, cwd, env = {}) {
  const safeEnv = Object.fromEntries(gitBaseEnv.filter(key => process.env[key] !== undefined).map(key => [key, process.env[key]]));
  // Never honor an operator's global/system helpers or a terminal prompt
  // while handling repository-controlled content. Callers may add the
  // temporary askpass variables needed during clone/fetch.
  Object.assign(safeEnv, {
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_TERMINAL_PROMPT: '0',
    GIT_OPTIONAL_LOCKS: '0'
  });
  // Keep enough headroom for a deliberately truncated diff plus git's stderr,
  // while still bounding memory for a pathological repository.
  return exec('git', args, { cwd, env: { ...safeEnv, ...env }, timeout: 120_000, maxBuffer: Math.max(64 * 1024 * 1024, MAX_DIFF_BYTES + 8 * 1024 * 1024) });
}

function cloneUrl(repo) {
  if (!validRepo(repo)) throw new Error('Invalid repository name');
  const host = process.env.GITHUB_HOST || 'github.com';
  if (!/^[A-Za-z0-9.-]+$/.test(host)) throw new Error('Invalid GitHub host');
  return `https://${host}/${repo}.git`;
}

async function checkout(repo, number, pr, token) {
  const root = await mkdtemp(join(tmpdir(), 'crow-pr-'));
  // Keep the installation token outside the worktree. The reviewer is given
  // the worktree and may inspect every readable file in it; a token placed in
  // its parent could otherwise be exposed by a prompt-injected model.
  let credentialRoot;
  try {
    credentialRoot = await mkdtemp(join(tmpdir(), 'crow-credentials-'));
  } catch (error) {
    await rm(root, { recursive: true, force: true });
    throw error;
  }
  const tokenFile = join(credentialRoot, 'token');
  const askpass = join(credentialRoot, 'askpass.sh');
  const env = { GIT_ASKPASS: askpass, CROW_TOKEN_FILE: tokenFile, GIT_TERMINAL_PROMPT: '0', GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' };
  const repoPath = join(root, 'repo');
  try {
    await writeFile(tokenFile, token, { mode: 0o600 });
    await writeFile(askpass, '#!/bin/sh\ncase "${1:-}" in\n  *sername*|*username*) printf "%s\\n" "x-access-token" ;;\n  *) cat "$CROW_TOKEN_FILE" ;;\nesac\n', { mode: 0o700 });
    try { await git(['clone', '--filter=blob:none', '--no-checkout', cloneUrl(repo), repoPath], root, env); }
    catch (error) {
      // Older GitHub Enterprise versions may not support partial clone. Retry
      // without the optimization, while preserving the original error for
      // authentication/network failures.
      if (!/filter|partial clone|promisor/i.test(error.message)) throw error;
      await rm(repoPath, { recursive: true, force: true });
      await git(['clone', '--no-checkout', cloneUrl(repo), repoPath], root, env);
    }
    // The pull ref also works for PRs opened from forks; the head SHA is not
    // always advertised by the base repository's normal branches.
    await git(['fetch', 'origin', pr.base.sha, `refs/pull/${number}/head:refs/remotes/origin/crow-pr`], repoPath, env);
    // Materialize tracked symlinks as ordinary files. A malicious PR can add
    // a link to a CLI auth file or another host path; the reviewer must never
    // be able to traverse such a link from its worktree.
    await git(['-c', 'core.symlinks=false', 'checkout', '--detach', pr.head.sha], repoPath, env);
    // Keep credentials around until the first diff has been materialized. A
    // partial clone can lazily fetch base-commit blobs during `git diff`; the
    // caller removes this directory immediately before starting the provider.
    return { root, repoPath, credentialRoot };
  } catch (error) {
    await Promise.all([
      rm(root, { recursive: true, force: true }),
      rm(credentialRoot, { recursive: true, force: true })
    ]);
    throw error;
  }
}

function decodeGitQuotedPath(value) {
  if (!(value.startsWith('"') && value.endsWith('"'))) return value;
  const bytes = [];
  const escapes = { a: 7, b: 8, t: 9, n: 10, v: 11, f: 12, r: 13, '\\': 92, '"': 34 };
  for (let index = 1; index < value.length - 1;) {
    const character = value[index++];
    if (character !== '\\') {
      bytes.push(...Buffer.from(character));
      continue;
    }
    if (index >= value.length - 1) { bytes.push(92); break; }
    const escaped = value[index++];
    if (/^[0-7]$/.test(escaped) && /^[0-7]{2}$/.test(value.slice(index, index + 2))) {
      bytes.push(parseInt(escaped + value.slice(index, index + 2), 8));
      index += 2;
    } else if (escaped === 'x' && /^[0-9a-f]{2}$/i.test(value.slice(index, index + 2))) {
      bytes.push(parseInt(value.slice(index, index + 2), 16));
      index += 2;
    } else if (Object.prototype.hasOwnProperty.call(escapes, escaped)) bytes.push(escapes[escaped]);
    else bytes.push(...Buffer.from(escaped));
  }
  return Buffer.from(bytes).toString('utf8');
}

export function changedLines(diff) {
  const lines = new Map();
  let path = null;
  let newLine = 0;
  let inHunk = false;
  for (const raw of String(diff).split('\n')) {
    if (raw.startsWith('diff --git ')) { path = null; newLine = 0; inHunk = false; continue; }
    // A newly-added source line can itself begin with "+++ ". Only interpret
    // that prefix as a file header before the first hunk for this file.
    if (!inHunk && raw.startsWith('+++ ')) {
      let value = raw.slice(4).split('\t', 1)[0];
      if (value === '/dev/null') { path = null; newLine = 0; continue; }
      value = decodeGitQuotedPath(value);
      path = value.startsWith('b/') ? value.slice(2) : null;
      continue;
    }
    const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/);
    if (hunk) { newLine = Number(hunk[1]); inHunk = true; continue; }
    if (!path || newLine < 1 || !inHunk) continue;
    // Git uses this metadata line for files without a trailing newline; it is
    // not a context line and therefore must not advance the right-side line.
    if (raw.startsWith('\\')) continue;
    if (raw.startsWith('+')) { if (!lines.has(path)) lines.set(path, new Set()); lines.get(path).add(newLine); newLine += 1; }
    else if (!raw.startsWith('-')) newLine += 1;
  }
  return lines;
}

function utf8Prefix(value, maxBytes) {
  const bytes = Buffer.from(String(value), 'utf8');
  let end = Math.max(0, Math.min(bytes.length, Math.floor(Number(maxBytes) || 0)));
  // If the byte boundary falls in the middle of a UTF-8 sequence, back up to
  // the previous code-point boundary instead of emitting U+FFFD and exceeding
  // the configured limit.
  while (end > 0 && end < bytes.length && (bytes[end] & 0xc0) === 0x80) end -= 1;
  return bytes.subarray(0, end).toString('utf8');
}

export function truncateDiff(value, maxBytes) {
  const text = String(value);
  const limit = Math.max(1, Math.floor(Number(maxBytes) || 1));
  if (Buffer.byteLength(text, 'utf8') <= limit) return text;
  const suffix = `\n\n[Diff truncated by Crow at ${limit} bytes.]`;
  const suffixBytes = Buffer.byteLength(suffix, 'utf8');
  // Avoid cutting a multi-byte character in half while keeping the complete
  // returned context at or below the configured byte limit. For very small
  // limits there is no room for the explanatory suffix, so return a safe
  // UTF-8 prefix only.
  if (suffixBytes >= limit) return utf8Prefix(text, limit);
  const clipped = utf8Prefix(text, limit - suffixBytes);
  return `${clipped}${suffix}`;
}

function validJob(job) {
  // RegExp#test coerces non-string values. Require actual SHA strings so a
  // malformed webhook cannot smuggle a numeric/object value through the
  // validation and into Git arguments or the durable queue key.
  return job && Number.isSafeInteger(job.installationId) && job.installationId > 0 && validRepo(job.repo) && Number.isSafeInteger(job.number) && job.number > 0 && typeof job.sha === 'string' && shaPattern.test(job.sha) && (job.baseSha === undefined || (typeof job.baseSha === 'string' && shaPattern.test(job.baseSha)));
}

function markerFor(sha, baseSha) { return `<!-- crow-review:${sha}${baseSha ? `:${baseSha}` : ''} -->`; }
function legacyMarkerFor(sha) { return `<!-- crow-review:${sha} -->`; }

async function alreadyPosted(repo, number, token, markers) {
  markers = Array.isArray(markers) ? markers : [markers];
  const [comments, reviews] = await Promise.all([listIssueComments(repo, number, token), listReviews(repo, number, token)]);
  const configuredBot = process.env.CROW_BOT_LOGIN?.trim().toLowerCase();
  return [...(comments || []), ...(reviews || [])].some(item => {
    if (typeof item?.body !== 'string' || !markers.some(marker => item.body.includes(marker))) return false;
    // Installation-token comments/reviews are authored by a GitHub Bot. If a
    // caller configures the exact login, prefer that; otherwise reject a
    // human-authored marker so anyone cannot suppress a review by pasting one.
    if (!item.user) return true; // tolerate minimal/legacy API fixtures
    const login = String(item.user.login || '').toLowerCase();
    if (configuredBot) return login === configuredBot;
    return item.user.type === 'Bot' || login.endsWith('[bot]');
  });
}

function summaryBody(result, provider, marker, inlineFindings = []) {
  const inlineKeys = new Set(inlineFindings.map(item => `${item.path}:${item.line}:${item.title}`));
  const remaining = result.findings.filter(item => !inlineKeys.has(`${item.path}:${item.line}:${item.title}`));
  const intro = result.findings.length ? `Found **${result.findings.length}** finding(s)${inlineFindings.length < result.findings.length ? ` (**${inlineFindings.length}** inline; the rest are summarized below)` : ''}.` : 'No actionable findings in this change.';
  const details = remaining.length ? `\n\n${remaining.map(item => '- **' + item.severity + '** `' + item.path + ':' + item.line + '` — ' + item.title + ': ' + item.body + (item.recommendation ? ' **Recommendation:** ' + item.recommendation : '')).join('\n')}` : '';
  const body = `${marker}\n## Crow review\n\n${result.summary}\n\n${intro}${details}\n\n_Reviewed automatically with ${provider === 'claude' ? 'Claude Code' : 'Codex CLI'}._`;
  // GitHub rejects oversized review/comment bodies. Keep the marker and the
  // beginning of the summary so a retry can still detect an already-posted
  // review.
  const maxBytes = 60_000;
  if (Buffer.byteLength(body, 'utf8') <= maxBytes) return body;
  const suffix = '\n\n_[Crow truncated this review body.]_';
  const available = Math.max(0, maxBytes - Buffer.byteLength(suffix, 'utf8'));
  return `${utf8Prefix(body, available)}${suffix}`;
}

function findingOnChangedLine(item, changed) {
  const candidates = [item.path];
  if (item.path.startsWith('./')) candidates.push(item.path.replace(/^\.\//, ''));
  if (item.path.startsWith('a/')) candidates.push(item.path.slice(2));
  if (item.path.startsWith('b/')) candidates.push(item.path.slice(2));
  const path = candidates.find(candidate => changed.get(candidate)?.has(item.line));
  return path ? { ...item, path } : undefined;
}

export async function processReview(job, { enqueue, provider = process.env.CROW_PROVIDER || 'codex' } = {}) {
  if (!validJob(job)) throw new Error('Invalid review job');
  const token = await installationToken(job.installationId);
  const pr = await getPullRequest(job.repo, job.number, token);
  if (!pr?.head?.sha) return { skipped: true, findings: 0 };
  if (pr.state && pr.state !== 'open') return { skipped: true, reopenable: true, findings: 0 };
  if (!shaPattern.test(pr.head.sha) || !shaPattern.test(pr.base?.sha || '')) throw new Error('GitHub returned an invalid commit SHA');
  if (pr.head.sha !== job.sha || (job.baseSha && job.baseSha !== pr.base.sha)) {
    if (enqueue) await enqueue({ ...job, sha: pr.head.sha, baseSha: pr.base.sha });
    return { skipped: true, stale: true, findings: 0 };
  }
  const checkoutResult = await checkout(job.repo, job.number, pr, token);
  try {
    const rawDiff = (await git(['diff', '--no-ext-diff', '--no-textconv', '--no-renames', '--unified=40', pr.base.sha, pr.head.sha], checkoutResult.repoPath)).stdout;
    const diff = truncateDiff(rawDiff, MAX_DIFF_BYTES);
    // The model must never be able to read the installation token, even if it
    // has a shell-capable tool. Remove the temporary askpass directory after
    // Git has finished any lazy object fetches needed for the diff.
    await rm(checkoutResult.credentialRoot, { recursive: true, force: true });
    const title = typeof pr.title === 'string' ? pr.title.slice(0, 2_000) : '';
    const description = typeof pr.body === 'string' ? pr.body.slice(0, 12_000) : '';
    const result = await runReview({ provider, cwd: checkoutResult.repoPath, timeoutMs: REVIEW_TIMEOUT_MS, maxOutputBytes: MAX_PROVIDER_OUTPUT_BYTES, redactionSecrets: [token, configuredSecret(), configuredPrivateKeyValue()], context: `Repository: ${job.repo}\nPull request: #${job.number}\nBase: ${pr.base.sha}\nHead: ${pr.head.sha}\n\nPR TITLE (untrusted text):\n${title}\n\nPR DESCRIPTION (untrusted text):\n${description}\n\nDIFF:\n${diff}` });
    const latest = await getPullRequest(job.repo, job.number, token);
    if (latest?.base?.sha && !shaPattern.test(latest.base.sha)) throw new Error('GitHub returned an invalid base commit SHA');
    if (latest?.state && latest.state !== 'open') return { skipped: true, reopenable: true, findings: result.findings.length };
    if (latest?.head?.sha !== pr.head.sha || (latest?.base?.sha && latest.base.sha !== pr.base.sha)) {
      if (enqueue && latest?.head?.sha && latest?.base?.sha) await enqueue({ ...job, sha: latest.head.sha, baseSha: latest.base.sha });
      return { skipped: true, stale: true, findings: result.findings.length };
    }
    const marker = markerFor(pr.head.sha, pr.base.sha);
    // A head-only marker predates base-SHA tracking. Check it only for legacy
    // jobs that also lack a base SHA; otherwise a changed base branch must be
    // allowed to receive a fresh review for the same head commit.
    const markers = job.baseSha ? [marker] : [marker, legacyMarkerFor(pr.head.sha)];
    if (await alreadyPosted(job.repo, job.number, token, markers)) return { skipped: true, duplicate: true, findings: result.findings.length };
    const changed = changedLines(diff);
    const normalizedResult = { ...result, findings: result.findings.map(item => findingOnChangedLine(item, changed) || item) };
    const safeFindings = normalizedResult.findings.filter(item => changed.get(item.path)?.has(item.line)).slice(0, 50);
    const body = summaryBody(normalizedResult, provider, marker, safeFindings);
    const comments = safeFindings.map(item => ({ path: item.path, line: item.line, side: 'RIGHT', body: `**${item.severity}** — ${item.title}\n\n${item.body}\n\n**Recommendation:** ${item.recommendation}` }));
    try { await postReview(job.repo, job.number, token, { commitId: pr.head.sha, body, comments }); }
    catch (error) {
      if (!(error instanceof GitHubError) || error.status !== 422) throw error;
      // A review can be rejected when one of the line comments is no longer
      // valid (for example after a force-push). The issue-comment fallback
      // must still include every finding, including those that were inline in
      // the attempted review.
      await postIssueComment(job.repo, job.number, token, summaryBody(normalizedResult, provider, marker));
    }
    return { skipped: false, findings: result.findings.length };
  } finally {
    await Promise.all([
      rm(checkoutResult.root, { recursive: true, force: true }),
      rm(checkoutResult.credentialRoot, { recursive: true, force: true })
    ]);
  }
}

function readBody(request, maxBytes) {
  return new Promise((resolve, reject) => {
    let size = 0;
    let rejected = false;
    const chunks = [];
    request.on('data', chunk => {
      if (rejected) return;
      size += chunk.length;
      if (size > maxBytes) {
        rejected = true;
        chunks.length = 0;
        // Drain the request instead of destroying the socket before the 413
        // response is written. Memory stays bounded while chunked requests
        // still receive a useful HTTP status.
        request.resume();
        reject(Object.assign(new Error('Request body too large'), { code: 'BODY_TOO_LARGE' }));
        return;
      }
      chunks.push(chunk);
    });
    request.on('end', () => { if (!rejected) resolve(Buffer.concat(chunks)); });
    request.on('error', error => { if (!rejected) reject(error); });
  });
}

export function createCrowServer({ store = new JobStore(), webhookSecret = configuredSecret(), maxQueue = MAX_QUEUE, provider = process.env.CROW_PROVIDER || 'codex', reviewer = processReview } = {}) {
  maxQueue = Math.max(1, Math.floor(positiveNumber(maxQueue, MAX_QUEUE)));
  provider = String(provider).trim().toLowerCase();
  if (!['codex', 'claude'].includes(provider)) throw new Error(`Unsupported CROW_PROVIDER: ${provider}`);
  let processing = false;
  let wakeTimer;

  const drain = async () => {
    if (processing) return;
    processing = true;
    try {
      while (true) {
        await store.promote?.(maxQueue);
        const job = await store.take();
        if (!job) break;
        try {
          const result = await reviewer(job, { enqueue, provider });
          if (result.reopenable && store.release) await store.release(job);
          else await store.complete(job, result.findings || 0);
          await store.promote?.(maxQueue);
          console.log(`[crow] ${result.skipped ? 'skipped' : 'reviewed'} ${job.repo}#${job.number}@${job.sha.slice(0, 7)} (${result.findings || 0} findings)`);
        } catch (error) {
          const attempt = await store.retry(job, error, 3, maxQueue);
          console.error(`[crow] review failed (attempt ${attempt}): ${error.message}`);
        }
      }
    } finally {
      processing = false;
      if (store.queuedCount || store.pendingCount) {
        const delay = Math.max(100, Math.min(30_000, (store.nextRetryAt || Date.now()) - Date.now()));
        clearTimeout(wakeTimer); wakeTimer = setTimeout(drain, delay);
      }
    }
  };

  async function enqueue(job) {
    if (!validJob(job)) throw new Error('Invalid review job');
    const accepted = await store.enqueue(job, maxQueue);
    if (accepted) void drain().catch(error => console.error(`[crow] queue drain failed: ${error.message}`));
    return accepted;
  }

  const json = (response, status, value) => { response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' }); response.end(JSON.stringify(value)); };
  const server = createServer(async (request, response) => {
    const url = new URL(request.url || '/', 'http://localhost');
    if (request.method === 'GET' && url.pathname === '/health') { json(response, 200, { ok: true, configured: Boolean(webhookSecret && process.env.GITHUB_APP_ID?.trim() && configuredPrivateKey()), provider, queued: store.queuedCount ?? store.pendingCount, processing }); return; }
    if (request.method === 'GET' && url.pathname === '/setup') {
      response.writeHead(200, { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' });
      response.end('Crow setup: create a private GitHub App with the pull_request webhook, install it on the repositories to review, then set GITHUB_APP_ID, GITHUB_PRIVATE_KEY_FILE, and GITHUB_WEBHOOK_SECRET_FILE in Crow. This endpoint is informational; no credentials are displayed here.');
      return;
    }
    if (request.method !== 'POST' || url.pathname !== '/webhooks/github') { response.writeHead(404); response.end('Not found'); return; }
    if (!webhookSecret) { json(response, 503, { error: 'Webhook secret is not configured' }); return; }
    if (Number(request.headers['content-length'] || 0) > MAX_WEBHOOK_BYTES) { json(response, 413, { error: 'Request body too large' }); return; }
    let raw;
    try { raw = await readBody(request, MAX_WEBHOOK_BYTES); } catch (error) { json(response, error.code === 'BODY_TOO_LARGE' ? 413 : 400, { error: error.message }); return; }
    if (!verifySignature(raw, request.headers['x-hub-signature-256'], webhookSecret)) { response.writeHead(401); response.end('Invalid signature'); return; }
    if (request.headers['x-github-event'] !== 'pull_request') { response.writeHead(202); response.end('Ignored'); return; }
    let payload;
    try { payload = JSON.parse(raw.toString('utf8')); } catch { json(response, 400, { error: 'Invalid JSON' }); return; }
    const action = payload?.action;
    const job = { installationId: Number(payload?.installation?.id), repo: payload?.repository?.full_name, number: Number(payload?.number), sha: payload?.pull_request?.head?.sha };
    if (payload?.pull_request?.base?.sha !== undefined) job.baseSha = payload.pull_request.base.sha;
    if (!['opened', 'reopened', 'synchronize'].includes(action) || !validJob(job)) { response.writeHead(202); response.end('Ignored'); return; }
    try {
      const accepted = await enqueue(job);
      json(response, 202, { accepted, queued: store.queuedCount ?? store.pendingCount });
    } catch (error) { json(response, 400, { error: error.message }); }
  });
  // Bound slowloris/idle HTTP connections; GitHub webhook deliveries are
  // small and should complete well within this window.
  server.requestTimeout = 60_000;
  server.headersTimeout = 15_000;
  server.keepAliveTimeout = 5_000;
  server.on('close', () => clearTimeout(wakeTimer));
  server.crow = { store, enqueue, drain, get processing() { return processing; } };
  return server;
}

export async function start() {
  const store = await new JobStore().load();
  const server = createCrowServer({ store });
  return new Promise((resolve, reject) => {
    const onError = error => { server.removeListener('listening', onListening); reject(error); };
    const onListening = () => {
      server.removeListener('error', onError);
      console.log(`[crow] listening on ${bindHost}:${port}`);
      // Do not burn through recovered jobs while credentials are missing;
      // operators can fix configuration and restart without losing them.
      const ready = Boolean(configuredSecret() && process.env.GITHUB_APP_ID?.trim() && configuredPrivateKey());
      if (ready) void server.crow.drain().catch(error => console.error(`[crow] queue drain failed: ${error.message}`));
      else if (server.crow.store.queuedCount) console.warn('[crow] credentials are not configured; queued reviews will wait for a restart');
      resolve(server);
    };
    server.once('error', onError);
    server.once('listening', onListening);
    server.listen(port, bindHost);
  });
}

const isMain = process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url;
if (isMain) start().catch(error => { console.error(`[crow] startup failed: ${error.message}`); process.exitCode = 1; });
