import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { processRun, cleanEnv, hash, isRecord, errorCode } from "./util.mjs";
import type { ProcessOptions } from "./util.mjs";
import type {
  InspectionSource,
  ReviewJob,
  GitHubPullRequest,
  Guidance,
} from "./types.mjs";
const sha = /^[0-9a-f]{40,64}$/;
export function revision(value: unknown): string {
  if (typeof value !== "string" || !sha.test(value))
    throw new Error("Invalid Git revision");
  return value;
}
export function safePath(value: unknown): string {
  if (
    typeof value !== "string" ||
    !value ||
    value.startsWith("/") ||
    value.includes("\\") ||
    /[\0\r\n]/.test(value) ||
    value.split("/").some((p) => p === ".." || p === "")
  )
    throw new Error("Invalid repository path");
  return value;
}
export async function git(
  dir: string,
  args: string[],
  extra: ProcessOptions = {},
) {
  return (
    await processRun(
      "git",
      [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.file.allow=never",
        "-c",
        "protocol.ext.allow=never",
        "-C",
        dir,
        ...args,
      ],
      { timeout: 120000, ...extra },
    )
  ).stdout;
}
export async function checkout(
  root: string,
  job: Pick<ReviewJob, "id" | "repo" | "number">,
  pr: GitHubPullRequest,
  token: string,
  { signal }: Pick<ProcessOptions, "signal"> = {},
): Promise<InspectionSource> {
  const dir = join(root, "sources", job.id);
  await mkdir(dir, { recursive: true, mode: 0o700 });
  await git(dir, ["init", "--bare", "."], { signal });
  // Token stays in the child's environment, never a URL, command argument, or Git config.
  const auth = Buffer.from(`x-access-token:${token}`).toString("base64");
  const env = cleanEnv({
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.https://github.com/.extraheader",
    GIT_CONFIG_VALUE_0: `AUTHORIZATION: basic ${auth}`,
  });
  await git(
    dir,
    [
      "fetch",
      "--no-tags",
      "--no-recurse-submodules",
      `https://github.com/${job.repo}.git`,
      revision(pr.base.sha),
      `refs/pull/${job.number}/head:refs/crow/head`,
    ],
    { env, signal },
  );
  const head = (
    await git(dir, ["rev-parse", "refs/crow/head"], { signal })
  ).trim();
  if (head !== pr.head.sha)
    throw Object.assign(new Error("PR changed while fetching"), {
      kind: "superseded",
    });
  const base = (
    await git(dir, ["merge-base", head, revision(pr.base.sha)], { signal })
  ).trim();
  return { dir, head, base, target: pr.base.ref, targetSha: pr.base.sha };
}
export async function files(source: InspectionSource, rev = source.head) {
  return (
    await git(source.dir, ["ls-tree", "-r", "--name-only", "-z", revision(rev)])
  )
    .split("\0")
    .filter(Boolean);
}
export async function readBlob(
  source: InspectionSource,
  path: string,
  rev = source.head,
) {
  safePath(path);
  revision(rev);
  const meta = await git(source.dir, ["ls-tree", rev, "--", path]);
  if (!/^100(644|755) blob [a-f0-9]+\t/.test(meta))
    throw new Error("Only regular tracked files can be read");
  const text = await git(source.dir, ["show", `${rev}:${path}`], {
    limit: 2 * 1024 * 1024,
  });
  if (text.includes("\0"))
    throw new Error("Binary file cannot be read as text");
  return text;
}
export async function diff(source: InspectionSource, path?: string) {
  const args = [
    "diff",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--unified=5",
    revision(source.base),
    revision(source.head),
  ];
  if (path) args.push("--", safePath(path));
  return git(source.dir, args);
}
async function changedFiles(source: InspectionSource) {
  return (
    await git(source.dir, [
      "diff",
      "--no-ext-diff",
      "--no-textconv",
      "--no-renames",
      "--name-only",
      "-z",
      revision(source.base),
      revision(source.head),
    ])
  )
    .split("\0")
    .filter(Boolean);
}
function pageBounds(args: Record<string, unknown>, maximum: number) {
  const offset = args.offset ?? 0;
  const count = args.count ?? maximum;
  if (
    typeof offset !== "number" ||
    !Number.isSafeInteger(offset) ||
    offset < 0 ||
    typeof count !== "number" ||
    !Number.isSafeInteger(count) ||
    count < 1 ||
    count > maximum
  )
    throw new Error(
      `offset must be a nonnegative integer and count must be between 1 and ${maximum}`,
    );
  return { offset, count };
}
function pageMetadata(total: number, offset: number, count: number) {
  const nextOffset = offset + count < total ? offset + count : null;
  return { offset, total, nextOffset, truncated: nextOffset !== null };
}
type InspectionPage = ReturnType<typeof pageMetadata> &
  ({ files: string[] } | { patch: string });
export async function guidance(source: InspectionSource): Promise<Guidance> {
  const names = await files(source, source.targetSha),
    wanted = names
      .filter(
        (x) =>
          x === "AGENTS.md" ||
          x.endsWith("/AGENTS.md") ||
          x === ".crow/review.md",
      )
      .sort();
  const parts = [];
  let size = 0;
  for (const path of wanted) {
    const body = await readBlob(source, path, source.targetSha);
    size += Buffer.byteLength(body);
    if (size > 256000)
      throw new Error(
        "Repository guidance exceeds 256 KB; reduce the instruction files",
      );
    parts.push({ path, body });
  }
  return { files: parts, fingerprint: hash(parts) };
}
export async function inspectionTool(
  source: InspectionSource,
  name: string,
  input: unknown,
): Promise<string | InspectionPage> {
  if (!isRecord(input))
    throw new Error("Inspection arguments must be an object");
  const args = input;
  if (name === "list_files") {
    const { offset, count } = pageBounds(args, 10000);
    if (args.prefix !== undefined && typeof args.prefix !== "string")
      throw new Error("prefix must be a string");
    if (
      args.changed_only !== undefined &&
      typeof args.changed_only !== "boolean"
    )
      throw new Error("changed_only must be a boolean");
    const prefix = typeof args.prefix === "string" ? args.prefix : "";
    const names = (
      await (args.changed_only ? changedFiles(source) : files(source))
    ).filter((path) => path.startsWith(prefix));
    return {
      files: names.slice(offset, offset + count),
      ...pageMetadata(names.length, offset, count),
    };
  }
  if (name === "read_file") {
    const text = await readBlob(
      source,
      safePath(args.path),
      args.revision === "base" ? source.base : source.head,
    );
    if (
      (args.start !== undefined && typeof args.start !== "number") ||
      (args.count !== undefined && typeof args.count !== "number")
    )
      throw new Error("Line start and count must be numbers");
    const start = Math.max(1, Math.trunc(args.start || 1)),
      count = Math.min(500, Math.max(1, Math.trunc(args.count || 200)));
    return text
      .split("\n")
      .slice(start - 1, start - 1 + count)
      .map((s, i) => `${start + i}: ${s}`)
      .join("\n");
  }
  if (name === "diff") {
    const { offset, count } = pageBounds(args, 200000);
    const patch = await diff(
      source,
      args.path ? safePath(args.path) : undefined,
    );
    return {
      patch: patch.slice(offset, offset + count),
      ...pageMetadata(patch.length, offset, count),
    };
  }
  if (name === "search") {
    if (
      typeof args.text !== "string" ||
      args.text.length < 1 ||
      args.text.length > 1000
    )
      throw new Error("Search needs 1–1000 literal characters");
    try {
      return (
        await git(
          source.dir,
          [
            "grep",
            "-n",
            "-I",
            "-F",
            "-e",
            args.text,
            revision(source.head),
            "--",
            ...(args.path ? [safePath(args.path)] : []),
          ],
          { limit: 2 * 1024 * 1024 },
        )
      ).slice(0, 100000);
    } catch (e) {
      if (errorCode(e) === 1) return "";
      throw e;
    }
  }
  throw new Error("Unknown inspection tool");
}
