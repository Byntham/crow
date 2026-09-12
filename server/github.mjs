import { createHmac, createSign, timingSafeEqual } from 'node:crypto';
import { readFileSync } from 'node:fs';

const apiRoot = (process.env.GITHUB_API_URL || 'https://api.github.com').replace(/\/+$/, '');

export class GitHubError extends Error {
  constructor(message, status) { super(message); this.name = 'GitHubError'; this.status = status; }
}

function base64url(value) { return Buffer.from(value).toString('base64url'); }
function repoPath(repo) {
  const [owner, name] = String(repo || '').split('/');
  if (!owner || !name || String(repo).split('/').length !== 2 || !/^[A-Za-z0-9_.-]+$/.test(owner) || !/^[A-Za-z0-9_.-]+$/.test(name) || owner === '.' || owner === '..' || name === '.' || name === '..') throw new Error('Invalid repository name');
  return `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`;
}

export function verifySignature(rawBody, signature, secret) {
  if (Array.isArray(signature)) signature = signature[0];
  if ((typeof rawBody !== 'string' && !Buffer.isBuffer(rawBody)) || typeof signature !== 'string' || !secret || !signature.startsWith('sha256=')) return false;
  const expected = Buffer.from(`sha256=${createHmac('sha256', secret).update(rawBody).digest('hex')}`);
  const received = Buffer.from(signature);
  return expected.length === received.length && timingSafeEqual(expected, received);
}

export function appJwt({ appId = process.env.GITHUB_APP_ID, privateKey = process.env.GITHUB_PRIVATE_KEY } = {}) {
  if (!privateKey && process.env.GITHUB_PRIVATE_KEY_FILE) privateKey = readFileSync(process.env.GITHUB_PRIVATE_KEY_FILE, 'utf8');
  if (!appId || !privateKey) throw new Error('GITHUB_APP_ID and GITHUB_PRIVATE_KEY are required');
  const key = privateKey.replace(/\\n/g, '\n');
  const now = Math.floor(Date.now() / 1000);
  const header = base64url(JSON.stringify({ alg: 'RS256', typ: 'JWT' }));
  const payload = base64url(JSON.stringify({ iat: now - 30, exp: now + 540, iss: String(appId) }));
  const input = `${header}.${payload}`;
  const signer = createSign('RSA-SHA256');
  signer.update(input);
  return `${input}.${signer.sign(key, 'base64url')}`;
}

async function github(path, { method = 'GET', token, body, timeoutMs = 20_000, retries = method === 'GET' ? 2 : 1 } = {}) {
  for (let attempt = 0; attempt <= retries; attempt += 1) {
    let response;
    try {
      response = await fetch(`${apiRoot}${path}`, {
        method,
        signal: AbortSignal.timeout(timeoutMs),
        headers: { Accept: 'application/vnd.github+json', 'User-Agent': 'Crow/0.1', ...(token ? { Authorization: `Bearer ${token}` } : {}), 'X-GitHub-Api-Version': '2022-11-28', ...(body ? { 'Content-Type': 'application/json' } : {}) },
        body: body ? JSON.stringify(body) : undefined
      });
    } catch (error) {
      if (attempt < retries) { await new Promise(resolve => setTimeout(resolve, 250 * 2 ** attempt)); continue; }
      throw new GitHubError(`GitHub request failed: ${error.message}`, 599);
    }
    const text = response.status === 204 ? '' : await response.text();
    if (response.ok) return text ? JSON.parse(text) : null;
    const retryable = [429, 500, 502, 503, 504].includes(response.status);
    if (retryable && attempt < retries) {
      const retryAfter = Number(response.headers?.get?.('retry-after'));
      await new Promise(resolve => setTimeout(resolve, Number.isFinite(retryAfter) ? Math.min(retryAfter * 1000, 10_000) : 250 * 2 ** attempt));
      continue;
    }
    throw new GitHubError(`GitHub ${method} ${path} failed (${response.status}): ${text.slice(0, 800)}`, response.status);
  }
}

export async function installationToken(installationId) {
  const result = await github(`/app/installations/${encodeURIComponent(installationId)}/access_tokens`, { method: 'POST', token: appJwt(), retries: 2 });
  if (!result || typeof result.token !== 'string' || !result.token) throw new GitHubError('GitHub did not return an installation token', 502);
  return result.token;
}

export async function getPullRequest(repo, number, token) { return github(`${repoPath(repo)}/pulls/${encodeURIComponent(number)}`, { token }); }

// Marker checks must look past the first page on busy pull requests. Keep a
// finite bound so a pathological thread cannot turn one review into an
// unbounded number of API calls.
async function listCollection(path, token, maxPages = 10) {
  const all = [];
  for (let page = 1; page <= maxPages; page += 1) {
    const suffix = page === 1 ? '?per_page=100' : `?per_page=100&page=${page}`;
    const items = await github(`${path}${suffix}`, { token });
    if (!Array.isArray(items)) break;
    all.push(...items);
    if (items.length < 100) break;
  }
  return all;
}

export async function listIssueComments(repo, number, token) { return listCollection(`${repoPath(repo)}/issues/${encodeURIComponent(number)}/comments`, token); }
export async function listReviews(repo, number, token) { return listCollection(`${repoPath(repo)}/pulls/${encodeURIComponent(number)}/reviews`, token); }

export async function postReview(repo, number, token, { commitId, body, comments = [] }) {
  return github(`${repoPath(repo)}/pulls/${encodeURIComponent(number)}/reviews`, { method: 'POST', token, retries: 0, body: { commit_id: commitId, body, event: 'COMMENT', comments } });
}

export async function postIssueComment(repo, number, token, body) {
  return github(`${repoPath(repo)}/issues/${encodeURIComponent(number)}/comments`, { method: 'POST', token, retries: 0, body: { body } });
}
