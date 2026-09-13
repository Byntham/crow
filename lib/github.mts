import { createSign, createHmac, type KeyObject } from "node:crypto";
import { equal, repoName, sleep } from "./util.mjs";
import type {
  GitHubApp,
  GitHubPullRequest,
  GitHubReview,
  GitHubComment,
  InlineComment,
} from "./types.mjs";

type Repository = string | { name: string };
type SigningApp = { id: string | number; pem: string | KeyObject };
type RequestOptions = {
  token?: string;
  method?: string;
  body?: unknown;
  withHeaders?: boolean;
};
type HeaderResponse = { data: unknown; headers: Headers };
type Delivery = {
  id: number;
  guid: string;
  status_code: number;
  redelivery: boolean;
  delivered_at: string;
};

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Unexpected GitHub object");
  return value as Record<string, unknown>;
}
function string(value: unknown): string {
  if (typeof value !== "string") throw new Error("Unexpected GitHub string");
  return value;
}
function number(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value))
    throw new Error("Unexpected GitHub integer");
  return value;
}
function boolean(value: unknown): boolean {
  if (typeof value !== "boolean") throw new Error("Unexpected GitHub boolean");
  return value;
}
function pullRequest(value: unknown): GitHubPullRequest {
  const p = object(value),
    user = object(p.user),
    head = object(p.head),
    base = object(p.base);
  return {
    number: number(p.number),
    state: string(p.state),
    draft: p.draft === undefined ? false : boolean(p.draft),
    user: { login: string(user.login) },
    head: { sha: string(head.sha) },
    base: { sha: string(base.sha), ref: string(base.ref) },
    ...(typeof p.title === "string" ? { title: p.title } : {}),
    ...(typeof p.body === "string" || p.body === null ? { body: p.body } : {}),
    ...(typeof p.created_at === "string" ? { created_at: p.created_at } : {}),
    ...(typeof p.updated_at === "string" ? { updated_at: p.updated_at } : {}),
  };
}
function comment(value: unknown): GitHubComment {
  const c = object(value),
    user = c.user === null ? null : object(c.user);
  return {
    id: number(c.id),
    body: c.body === undefined ? "" : string(c.body),
    user: user === null ? null : { id: number(user.id) },
  };
}
function review(value: unknown): GitHubReview {
  const r = object(value),
    user = r.user === null ? null : object(r.user);
  return {
    id: number(r.id),
    body: r.body === null ? "" : string(r.body),
    user: user === null ? null : { id: number(user.id) },
    html_url: string(r.html_url),
  };
}
function delivery(value: unknown): Delivery {
  const d = object(value);
  return {
    id: number(d.id),
    guid: string(d.guid),
    status_code: number(d.status_code),
    redelivery: boolean(d.redelivery),
    delivered_at: string(d.delivered_at),
  };
}
export class GitHubError extends Error {
  status: number;
  retryAfter: number;
  constructor(message: string, status: number, retryAfter: number) {
    super(message);
    this.status = status;
    this.retryAfter = retryAfter;
  }
}

export function verify(
  raw: string | Buffer,
  signature: string | undefined,
  secret: string | undefined,
) {
  return (
    !!secret &&
    equal(
      `sha256=${createHmac("sha256", secret).update(raw).digest("hex")}`,
      signature,
    )
  );
}
export function jwt(app: SigningApp) {
  const enc = (x: unknown) =>
      Buffer.from(JSON.stringify(x)).toString("base64url"),
    now = Math.floor(Date.now() / 1000);
  const s = `${enc({ alg: "RS256", typ: "JWT" })}.${enc({ iat: now - 30, exp: now + 540, iss: String(app.id) })}`;
  return `${s}.${createSign("RSA-SHA256").update(s).sign(app.pem, "base64url")}`;
}
export class GitHub {
  app: GitHubApp | null;
  fetcher: typeof fetch;
  origin: string;
  constructor(
    app: GitHubApp | null,
    {
      fetcher = fetch,
      origin = "https://api.github.com",
    }: { fetcher?: typeof fetch; origin?: string } = {},
  ) {
    this.app = app;
    this.fetcher = fetcher;
    this.origin = origin;
  }
  private requireApp(): GitHubApp {
    if (!this.app) throw new Error("Complete GitHub App setup first");
    return this.app;
  }
  request(
    path: string,
    options: RequestOptions & { withHeaders: true },
  ): Promise<HeaderResponse>;
  request(
    path: string,
    options?: RequestOptions & { withHeaders?: false },
  ): Promise<unknown>;
  async request(
    path: string,
    { token, method = "GET", body, withHeaders = false }: RequestOptions = {},
  ): Promise<unknown | HeaderResponse> {
    const r = await this.fetcher(this.origin + path, {
      method,
      signal: AbortSignal.timeout(30000),
      headers: {
        Accept: "application/vnd.github+json",
        "User-Agent": "Crow/0.2",
        "X-GitHub-Api-Version": "2022-11-28",
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
        ...(body ? { "Content-Type": "application/json" } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) {
      throw new GitHubError(
        `GitHub ${method} ${path.split("?")[0]} returned ${r.status}`,
        r.status,
        Number(r.headers.get("retry-after") || 0) * 1000,
      );
    }
    const data: unknown = r.status === 204 ? null : await r.json();
    return withHeaders ? { data, headers: r.headers } : data;
  }
  async list(path: string, token?: string): Promise<unknown[]> {
    const result: unknown[] = [];
    for (let page = 1; page <= 1000; page++) {
      const items = await this.request(
        `${path}${path.includes("?") ? "&" : "?"}per_page=100&page=${page}`,
        { token },
      );
      const xs = Array.isArray(items)
        ? items
        : object(items).repositories || object(items).installations;
      if (!Array.isArray(xs)) throw new Error("Unexpected GitHub collection");
      result.push(...xs);
      if (xs.length < 100) return result;
    }
    throw new Error("GitHub pagination limit reached");
  }
  async token(repo: {
    name: string;
    installation: number | string;
  }): Promise<string> {
    const result = await this.request(
      `/app/installations/${repo.installation}/access_tokens`,
      {
        method: "POST",
        token: jwt(this.requireApp()),
        body: {
          repositories: [repo.name.split("/")[1]],
          permissions: {
            contents: "read",
            pull_requests: "write",
            issues: "write",
            metadata: "read",
          },
        },
      },
    );
    return string(object(result).token);
  }
  path(repo: Repository) {
    return `/repos/${repoName(typeof repo === "string" ? repo : repo.name)}`;
  }
  async pr(
    repo: Repository,
    n: number,
    token: string,
  ): Promise<GitHubPullRequest> {
    return pullRequest(
      await this.request(`${this.path(repo)}/pulls/${n}`, { token }),
    );
  }
  async prs(repo: Repository, token: string): Promise<GitHubPullRequest[]> {
    return (await this.list(`${this.path(repo)}/pulls?state=open`, token)).map(
      pullRequest,
    );
  }
  async comments(
    repo: Repository,
    n: number,
    token: string,
  ): Promise<GitHubComment[]> {
    return (
      await this.list(`${this.path(repo)}/issues/${n}/comments`, token)
    ).map(comment);
  }
  async reviews(
    repo: Repository,
    n: number,
    token: string,
  ): Promise<GitHubReview[]> {
    return (
      await this.list(`${this.path(repo)}/pulls/${n}/reviews`, token)
    ).map(review);
  }
  async botId() {
    const app = await this.request("/app", { token: jwt(this.requireApp()) });
    const user = await this.request(
      `/users/${encodeURIComponent(string(object(app).slug) + "[bot]")}`,
    );
    return number(object(user).id);
  }
  async installation(name: string): Promise<{ id: number }> {
    const result = await this.request(`${this.path(name)}/installation`, {
      token: jwt(this.requireApp()),
    });
    return { id: number(object(result).id) };
  }
  async status(
    repo: Repository,
    n: number,
    token: string,
    body: string,
    botId: number,
    knownId?: number,
  ): Promise<{ id: number }> {
    if (!knownId)
      knownId = (await this.comments(repo, n, token)).find(
        (c) =>
          c.user?.id === botId && c.body.startsWith("<!-- crow-status:v1 -->"),
      )?.id;
    if (knownId) {
      try {
        const result = await this.request(
          `${this.path(repo)}/issues/comments/${knownId}`,
          { token, method: "PATCH", body: { body } },
        );
        return { id: number(object(result).id) };
      } catch (e) {
        if (!(e instanceof GitHubError) || e.status !== 404) throw e;
      }
    }
    const result = await this.request(
      `${this.path(repo)}/issues/${n}/comments`,
      {
        token,
        method: "POST",
        body: { body },
      },
    );
    return { id: number(object(result).id) };
  }
  async publish(
    repo: Repository,
    n: number,
    token: string,
    body: string,
    head: string,
    comments: InlineComment[],
  ): Promise<Pick<GitHubReview, "id" | "html_url">> {
    const result = object(
      await this.request(`${this.path(repo)}/pulls/${n}/reviews`, {
        token,
        method: "POST",
        body: { commit_id: head, event: "COMMENT", body, comments },
      }),
    );
    return { id: number(result.id), html_url: string(result.html_url) };
  }
  async deliveries(token: string): Promise<Delivery[]> {
    const result: Delivery[] = [],
      seen = new Set<string>();
    let path: string | null = "/app/hook/deliveries?per_page=100";
    while (path) {
      if (seen.has(path) || seen.size >= 1000)
        throw new Error("GitHub delivery pagination did not terminate");
      seen.add(path);
      const { data, headers } = await this.request(path, {
        token,
        withHeaders: true,
      });
      if (!Array.isArray(data))
        throw new Error("Unexpected GitHub delivery collection");
      result.push(...data.map(delivery));
      const next = (headers.get("link") || "")
        .split(",")
        .find((part) => /;\s*rel="next"/.test(part));
      if (!next) {
        path = null;
        continue;
      }
      const match = next.match(/<([^>]+)>/);
      if (!match) throw new Error("Invalid GitHub delivery pagination link");
      const url = new URL(match[1], this.origin);
      // Never send an App bearer token to an origin supplied by a response header.
      if (
        url.origin !== new URL(this.origin).origin ||
        url.pathname !== "/app/hook/deliveries" ||
        url.username ||
        url.password
      )
        throw new Error("Invalid GitHub delivery pagination origin");
      path = url.pathname + url.search;
    }
    return result;
  }
  async audit() {
    const token = jwt(this.requireApp()),
      deliveries = await this.deliveries(token);
    const recovered = new Set(
      deliveries
        .filter((d) => d.status_code >= 200 && d.status_code < 300 && d.guid)
        .map((d) => d.guid),
    );
    for (const d of deliveries)
      if (
        (d.status_code === 0 || d.status_code >= 400) &&
        !d.redelivery &&
        !recovered.has(d.guid) &&
        Date.now() - Date.parse(d.delivered_at) < 3 * 86400000
      ) {
        if (!Number.isSafeInteger(d.id) || d.id < 1)
          throw new Error("Invalid GitHub delivery identifier");
        await this.request(`/app/hook/deliveries/${d.id}/attempts`, {
          token,
          method: "POST",
        });
        await sleep(100);
      }
  }
}
