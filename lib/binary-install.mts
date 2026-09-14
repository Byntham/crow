import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  readlink,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { gunzipSync } from "node:zlib";
import { errorCode, errorMessage, id, processRun } from "./util.mjs";
import type { ProcessRunner } from "./util.mjs";

const downloadOrigin = "https://downloads.birdapp.dev";
const maxArchive = 256 * 1024 * 1024;
const maxExtracted = 512 * 1024 * 1024;
interface ReleaseOptions {
  run?: ProcessRunner;
  fetch?: typeof globalThis.fetch;
  platform?: string;
  arch?: string;
}
function versionParts(version: string) {
  if (
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(version)?.[0] !==
    version
  )
    throw new Error(`Invalid stable Crow release version: ${version}`);
  const parts = version.split(".").map(Number);
  if (parts.some((part) => !Number.isSafeInteger(part)))
    throw new Error("Invalid release version");
  return parts;
}
function newer(next: string, current: string) {
  const a = versionParts(next),
    b = versionParts(current);
  for (let index = 0; index < a.length; index++)
    if (a[index] !== b[index]) return a[index]! > b[index]!;
  return false;
}
export function binaryPath(root: string) {
  return join(resolve(root), "current", "crow");
}
async function digest(file: string) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}
async function linkTarget(path: string) {
  try {
    if (!(await lstat(path)).isSymbolicLink())
      throw new Error(`Refusing to replace non-symlink ${path}`);
    return await readlink(path);
  } catch (error) {
    if (errorCode(error) === "ENOENT") return null;
    throw error;
  }
}
async function replaceLink(path: string, target: string) {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${id()}.tmp`;
  try {
    await symlink(target, temporary);
    await rename(temporary, path);
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } finally {
    await rm(temporary, { force: true });
  }
}
async function stageBinary(root: string, executable: string, version: string) {
  versionParts(version);
  root = resolve(root);
  const source = await stat(executable);
  if (!source.isFile() || source.size === 0 || source.size > maxExtracted)
    throw new Error(
      "Crow executable must be a nonempty regular file within the release size limit",
    );
  await mkdir(join(root, "releases"), { recursive: true, mode: 0o700 });
  const expected = await digest(executable);
  const directory = join(
    root,
    "releases",
    `${version}-${expected.slice(0, 16)}`,
  );
  const target = join(directory, "crow");
  try {
    if (
      (await lstat(directory)).isSymbolicLink() ||
      !(await lstat(target)).isFile() ||
      (await digest(target)) !== expected
    )
      throw new Error("Existing Crow release has unexpected contents");
    return target;
  } catch (error) {
    if (errorCode(error) !== "ENOENT") throw error;
  }
  const temporary = await mkdtemp(join(root, "releases", ".install-"));
  try {
    await copyFile(executable, join(temporary, "crow"));
    if ((await digest(join(temporary, "crow"))) !== expected)
      throw new Error("Crow executable changed during installation");
    await chmod(join(temporary, "crow"), 0o755);
    const file = await open(join(temporary, "crow"), "r");
    try {
      await file.sync();
    } finally {
      await file.close();
    }
    await rename(temporary, directory);
    const releases = await open(join(root, "releases"), "r");
    try {
      await releases.sync();
    } finally {
      await releases.close();
    }
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
  return target;
}
async function activate(root: string, executable: string) {
  const current = join(resolve(root), "current"),
    previous = await linkTarget(current);
  await replaceLink(current, dirname(executable));
  return async () => {
    if ((await linkTarget(current)) !== dirname(executable))
      throw new Error("Crow installation changed again; refusing rollback");
    if (previous === null) await rm(current);
    else await replaceLink(current, previous);
  };
}
export async function installBinary(
  root: string,
  options: {
    executable?: string;
    version: string;
    binDir?: string;
    allowSourceMigration?: boolean;
  },
) {
  root = resolve(root);
  const entry = join(
    resolve(options.binDir ?? join(homedir(), ".local", "bin")),
    "crow",
  );
  try {
    const info = await lstat(entry);
    if (info.isSymbolicLink()) {
      if (resolve(dirname(entry), await readlink(entry)) !== binaryPath(root))
        throw new Error(`Refusing to replace unrelated executable ${entry}`);
    } else {
      const text =
        info.isFile() && info.size < 8192 ? await readFile(entry, "utf8") : "";
      const recognizable =
        text.startsWith(
          "#!/usr/bin/env bash\nset -euo pipefail\nexport PATH=",
        ) &&
        text.includes("/current/bin/crow.mjs") &&
        text.endsWith(' "$@"\n');
      if (!options.allowSourceMigration || !recognizable)
        throw new Error(
          `Refusing to replace existing executable ${entry}; move it aside first`,
        );
    }
  } catch (error) {
    if (errorCode(error) !== "ENOENT") throw error;
  }
  await linkTarget(join(root, "current"));
  const executable = await stageBinary(
    root,
    options.executable ?? process.execPath,
    options.version,
  );
  const rollback = await activate(root, executable);
  try {
    await replaceLink(entry, binaryPath(root));
  } catch (error) {
    await rollback();
    throw error;
  }
  return binaryPath(root);
}
async function publicDownload(
  url: string,
  options: ReleaseOptions,
  limit: number,
) {
  const response = await (options.fetch ?? globalThis.fetch)(url, {
    signal: AbortSignal.timeout(120_000),
    redirect: "error",
    headers: { "User-Agent": "Crow", Accept: "application/octet-stream" },
  });
  if (!response.ok || !response.body)
    throw new Error(`Crow release download failed (${response.status})`);
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of response.body) {
    size += chunk.length;
    if (size > limit) {
      throw new Error("Crow release exceeds its size limit");
    }
    chunks.push(Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}
async function latest(options: ReleaseOptions) {
  const metadata = (
    await publicDownload(`${downloadOrigin}/latest.txt`, options, 128)
  ).toString("utf8");
  const version = metadata.endsWith("\n") ? metadata.slice(0, -1) : metadata;
  versionParts(version);
  return { version, tag: `v${version}` };
}
export async function checkBinaryUpdate(
  currentVersion: string,
  options: ReleaseOptions = {},
) {
  try {
    const release = await latest(options);
    return {
      available: newer(release.version, currentVersion),
      version: release.version,
    };
  } catch (error) {
    return { available: false, warning: errorMessage(error) };
  }
}
function extractBinary(archive: Buffer) {
  const tar = gunzipSync(archive, { maxOutputLength: maxExtracted });
  let binary: Buffer | undefined;
  const names = new Set<string>();
  for (let offset = 0; offset + 512 <= tar.length;) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const field = (start: number, length: number) =>
      header
        .subarray(start, start + length)
        .toString("utf8")
        .split("\0")[0]!;
    const rawName = field(0, 100),
      prefix = field(345, 155),
      kind = field(156, 1);
    const name = rawName.startsWith("./") ? rawName.slice(2) : rawName;
    if (
      prefix ||
      ![
        "crow",
        "THIRD_PARTY_NOTICES",
        "LICENSE",
        "LICENSE.txt",
        "NODE-LICENSE",
        "NODE-LICENSE.txt",
      ].includes(name) ||
      names.has(name) ||
      !["", "0"].includes(kind)
    )
      throw new Error(
        "Crow release archive contains an unexpected path or non-regular file",
      );
    names.add(name);
    const sizeText = field(124, 12).trim();
    if (!/^[0-7]+$/.test(sizeText))
      throw new Error("Invalid release archive size");
    const size = parseInt(sizeText, 8);
    const checksum = parseInt(field(148, 8).trim(), 8);
    const actual = header.reduce(
      (sum, byte, index) => sum + (index >= 148 && index < 156 ? 32 : byte),
      0,
    );
    if (checksum !== actual || offset + 512 + size > tar.length)
      throw new Error("Invalid or truncated release archive");
    if (name === "crow")
      binary = tar.subarray(offset + 512, offset + 512 + size);
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  if (!binary?.length)
    throw new Error("Crow release archive has no executable");
  return binary;
}
export async function prepareBinaryUpdate(
  root: string,
  currentVersion: string,
  options: ReleaseOptions = {},
) {
  const platform = options.platform ?? process.platform,
    arch = options.arch ?? process.arch;
  if (platform !== "linux" || !["x64", "arm64"].includes(arch))
    throw new Error("Crow binaries support Linux x64 and arm64");
  const release = await latest(options);
  if (!newer(release.version, currentVersion)) return null;
  root = resolve(root);
  await mkdir(root, { recursive: true, mode: 0o700 });
  const temporary = await mkdtemp(join(root, ".update-"));
  const archiveName = `crow-${release.tag}-linux-${arch}.tar.gz`;
  try {
    for (const name of [archiveName, "SHA256SUMS"])
      await writeFile(
        join(temporary, name),
        await publicDownload(
          `${downloadOrigin}/releases/${release.tag}/${name}`,
          options,
          name === "SHA256SUMS" ? 1024 * 1024 : maxArchive,
        ),
        { mode: 0o600 },
      );
    const sums = (await readFile(join(temporary, "SHA256SUMS"), "utf8"))
      .split(/\r?\n/)
      .map((line) => /^([a-fA-F0-9]{64}) [ *](\S+)$/.exec(line))
      .filter((match) => match?.[2] === archiveName);
    if (
      sums.length !== 1 ||
      sums[0]![1]!.toLowerCase() !==
        (await digest(join(temporary, archiveName)))
    )
      throw new Error("Crow release checksum verification failed");
    const candidate = join(temporary, "crow");
    const binary = extractBinary(await readFile(join(temporary, archiveName)));
    if (
      binary.length < 20 ||
      !binary.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46])) ||
      binary[4] !== 2 ||
      binary[5] !== 1 ||
      binary.readUInt16LE(18) !== (arch === "x64" ? 62 : 183)
    )
      throw new Error(
        "Downloaded Crow executable does not match the requested Linux architecture",
      );
    await writeFile(candidate, binary, { mode: 0o755 });
    const result = await (options.run ?? processRun)(candidate, ["--version"], {
      timeout: 30_000,
      limit: 4096,
    });
    if (
      ![
        release.version,
        `crow ${release.version}`,
        `Crow ${release.version}`,
      ].includes(result.stdout.trim())
    )
      throw new Error(
        "Downloaded Crow executable reported an unexpected version",
      );
    const executable = await stageBinary(root, candidate, release.version);
    return {
      version: release.version,
      executable,
      activate: () => activate(root, executable),
    };
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
