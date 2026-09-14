import { createHash } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  rename,
  rm,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { cleanEnv, id, isRecord, processRun } from "./util.mjs";
import type { ProcessRunner } from "./util.mjs";

const releaseApi = "https://api.github.com/repos/openai/codex/releases/latest";
const requiredFiles = [
  "bin/codex",
  "bin/codex-code-mode-host",
  "codex-path/rg",
  "codex-resources/bwrap",
  "codex-package.json",
];
const optionalFiles = ["codex-resources/zsh/bin/zsh"];
const directories = new Set([
  "bin/",
  "codex-path/",
  "codex-resources/",
  "codex-resources/zsh/",
  "codex-resources/zsh/bin/",
]);
const maxArchiveBytes = 512 * 1024 * 1024;

interface InstallOptions {
  run?: ProcessRunner;
  fetcher?: typeof fetch;
  platform?: string;
  arch?: string;
}

/** Install the current official standalone package, including Code Mode helpers. */
export async function installCodex(
  root: string,
  {
    run = processRun,
    fetcher = fetch,
    platform = process.platform,
    arch = process.arch,
  }: InstallOptions = {},
): Promise<string> {
  if (platform !== "linux" || !["x64", "arm64"].includes(arch))
    throw new Error(
      "Automatic Codex installation supports Linux x64 and arm64. Install official Codex manually on this platform.",
    );
  const target = `${arch === "x64" ? "x86_64" : "aarch64"}-unknown-linux-musl`;
  const assetName = `codex-package-${target}.tar.gz`;
  const response = await fetcher(releaseApi, {
    headers: { Accept: "application/vnd.github+json", "User-Agent": "Crow" },
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok)
    throw new Error(
      `Could not retrieve the latest official Codex release: HTTP ${response.status}.`,
    );
  const release: unknown = await response.json();
  if (
    !isRecord(release) ||
    typeof release.tag_name !== "string" ||
    !/^rust-v\d+\.\d+\.\d+$/.test(release.tag_name) ||
    release.draft === true ||
    release.prerelease === true ||
    !Array.isArray(release.assets)
  )
    throw new Error("GitHub returned invalid official Codex release metadata.");
  const matches = release.assets.filter(
    (item: unknown) => isRecord(item) && item.name === assetName,
  );
  const asset: unknown = matches[0];
  const expectedUrl = `https://github.com/openai/codex/releases/download/${release.tag_name}/${assetName}`;
  if (
    matches.length !== 1 ||
    !isRecord(asset) ||
    asset.browser_download_url !== expectedUrl ||
    typeof asset.digest !== "string" ||
    !/^sha256:[a-f0-9]{64}$/.test(asset.digest) ||
    typeof asset.size !== "number" ||
    !Number.isSafeInteger(asset.size) ||
    asset.size <= 0 ||
    asset.size > maxArchiveBytes
  )
    throw new Error(
      "The latest official Codex package is missing a valid download URL, size, or published SHA-256 digest.",
    );

  const tools = resolve(root, "tools");
  await mkdir(tools, { recursive: true, mode: 0o700 });
  const stage = await mkdtemp(join(tools, ".codex-install-"));
  const archive = join(stage, "package.tar.gz");
  const unpacked = join(stage, "package");
  try {
    const download = await fetcher(expectedUrl, {
      signal: AbortSignal.timeout(300_000),
      headers: { "User-Agent": "Crow" },
    });
    if (!download.ok || !download.body)
      throw new Error(
        `Could not download the official Codex package: HTTP ${download.status}.`,
      );
    const checksum = createHash("sha256");
    const handle = await open(archive, "wx", 0o600);
    let bytes = 0;
    try {
      for await (const chunk of download.body) {
        bytes += chunk.byteLength;
        if (bytes > asset.size)
          throw new Error("Codex package download exceeds its published size.");
        checksum.update(chunk);
        await handle.writeFile(chunk);
      }
      await handle.sync();
    } finally {
      await handle.close();
    }
    if (
      bytes !== asset.size ||
      `sha256:${checksum.digest("hex")}` !== asset.digest
    )
      throw new Error(
        "Codex package SHA-256 checksum or size does not match the official release. Nothing was installed.",
      );

    // Only known regular files may be extracted. Reject links, duplicate names,
    // traversal, and unknown entries before tar writes any package paths.
    const options = {
      env: cleanEnv({ LC_ALL: "C" }),
      timeout: 120_000,
      limit: 64 * 1024,
    };
    const names = (await run("tar", ["-tzf", archive], options)).stdout
      .trimEnd()
      .split("\n");
    const entries = (await run("tar", ["-tvzf", archive], options)).stdout
      .trimEnd()
      .split("\n");
    const allowedFiles = new Set([...requiredFiles, ...optionalFiles]);
    const seen = new Set<string>();
    if (names.length !== entries.length)
      throw new Error("Unexpected Codex archive listing.");
    for (const [index, name] of names.entries()) {
      const kind = entries[index]?.[0];
      if (
        seen.has(name) ||
        !(
          (allowedFiles.has(name) && kind === "-") ||
          (directories.has(name) && kind === "d")
        )
      )
        throw new Error(`Unexpected or unsafe Codex archive entry: ${name}.`);
      seen.add(name);
    }
    for (const name of requiredFiles)
      if (!seen.has(name)) throw new Error(`Codex package is missing ${name}.`);
    await mkdir(unpacked, { mode: 0o700 });
    const files = [...allowedFiles].filter((name) => seen.has(name));
    await run(
      "tar",
      [
        "-xzf",
        archive,
        "--no-same-owner",
        "--no-same-permissions",
        "-C",
        unpacked,
        "--",
        ...files,
      ],
      options,
    );
    for (const name of files) {
      const path = join(unpacked, name);
      if (!(await lstat(path)).isFile())
        throw new Error(`Codex package contains a non-file: ${name}.`);
      await chmod(path, name === "codex-package.json" ? 0o600 : 0o700);
    }
    const metadata: unknown = JSON.parse(
      await readFile(join(unpacked, "codex-package.json"), "utf8"),
    );
    const version = release.tag_name.slice("rust-v".length);
    if (
      !isRecord(metadata) ||
      metadata.layoutVersion !== 1 ||
      metadata.version !== version ||
      metadata.target !== target ||
      metadata.variant !== "codex" ||
      metadata.entrypoint !== "bin/codex" ||
      metadata.resourcesDir !== "codex-resources" ||
      metadata.pathDir !== "codex-path"
    )
      throw new Error(
        "Codex package metadata does not match the requested release.",
      );
    const executable = join(unpacked, "bin/codex");
    const installedVersion = (
      await run(executable, ["--version"], { timeout: 30_000 })
    ).stdout.trim();
    if (installedVersion !== `codex-cli ${version}`)
      throw new Error(
        "Installed Codex executable reported an unexpected version.",
      );
    const destination = join(tools, `codex-${version}-${target}-${id()}`);
    await rename(unpacked, destination);
    return join(destination, "bin/codex");
  } finally {
    await rm(stage, { recursive: true, force: true });
  }
}
