#!/usr/bin/env node
// Runs only from the manually dispatched publication workflow. Never execute
// downloaded binaries in the job that holds Cloudflare credentials.
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

const execute = promisify(execFile);
const architectures = ["x64", "arm64"];
const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const maximumArchive = 160 * 1024 * 1024;
const maximumUnpacked = 256 * 1024 * 1024;

export function validateRequest({ version, commit, runId, confirmation }) {
  if (!versionPattern.test(version || "")) throw new Error("Use a stable VERSION such as 0.2.0.");
  if (!/^[a-f0-9]{40}$/.test(commit || "")) throw new Error("Provide the full 40-character build commit SHA.");
  if (!/^[1-9]\d*$/.test(runId || "")) throw new Error("Provide a numeric build run ID.");
  if (confirmation !== `publish ${version}`) throw new Error(`Confirmation must be exactly: publish ${version}`);
}

export function validateBuildRun(run, { repository, commit, runId }) {
  if (String(run.id) !== runId || run.repository?.full_name !== repository ||
      run.head_repository?.full_name !== repository || run.head_sha !== commit ||
      run.path !== ".github/workflows/release.yml" || run.status !== "completed" ||
      run.conclusion !== "success" || !["push", "workflow_dispatch"].includes(run.event)) {
    throw new Error("The selected run must be a successful Crow binary build from this repository at the exact requested commit.");
  }
}

export function validateArchive(bytes, architecture) {
  if (bytes.length > maximumArchive) throw new Error("Release archive exceeds the size limit.");
  const tar = gunzipSync(bytes, { maxOutputLength: maximumUnpacked });
  const names = new Set();
  let offset = 0;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const field = (start, length) => header.subarray(start, start + length).toString("utf8").replace(/\0.*$/s, "");
    const number = (start, length) => {
      const value = field(start, length).trim();
      if (!/^[0-7]+$/.test(value)) throw new Error("Invalid tar numeric field.");
      return parseInt(value, 8);
    };
    const expected = number(148, 8);
    const checksum = header.reduce((sum, byte, index) => sum + (index >= 148 && index < 156 ? 32 : byte), 0);
    const name = field(0, 100);
    const size = number(124, 12);
    if (checksum !== expected || ![0, 48].includes(header[156]) || field(345, 155) ||
        !["crow", "THIRD_PARTY_NOTICES"].includes(name) || names.has(name) ||
        size < 1 || offset + 512 + size > tar.length) throw new Error("Unexpected or malformed release archive entry.");
    names.add(name);
    if (name === "crow") {
      const binary = tar.subarray(offset + 512, offset + 512 + size);
      if (binary.length < 64 || !binary.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70])) ||
          binary[4] !== 2 || binary[5] !== 1 || binary.readUInt16LE(18) !== (architecture === "x64" ? 62 : 183)) {
        throw new Error(`Release does not contain a Linux ${architecture} ELF64 executable.`);
      }
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  if (names.size !== 2 || tar.length - offset < 1024 || tar.subarray(offset).some((byte) => byte !== 0)) {
    throw new Error("Incomplete release archive or unexpected trailing data.");
  }
}

export async function assembleRelease(directory, { version, commit }) {
  const expectedDirectories = architectures.map((arch) => `crow-linux-${arch}`);
  const entries = (await readdir(directory)).sort();
  if (JSON.stringify(entries) !== JSON.stringify([...expectedDirectories].sort())) {
    throw new Error("Expected exactly the x64 and arm64 build artifacts.");
  }
  const files = [];
  let checksums = "";
  for (const arch of architectures) {
    const artifact = join(directory, `crow-linux-${arch}`);
    const archive = `crow-v${version}-linux-${arch}.tar.gz`;
    const members = (await readdir(artifact)).sort();
    if (JSON.stringify(members) !== JSON.stringify([archive, "SHA256SUMS", "build-metadata.json"].sort())) {
      throw new Error(`Unexpected files in the ${arch} artifact.`);
    }
    const bytes = await readFile(join(artifact, archive));
    const sha256 = digest(bytes);
    const line = `${sha256}  ${archive}\n`;
    if (await readFile(join(artifact, "SHA256SUMS"), "utf8") !== line) throw new Error(`Checksum mismatch for ${archive}.`);
    const metadata = JSON.parse(await readFile(join(artifact, "build-metadata.json"), "utf8"));
    if (metadata.version !== version || metadata.commit !== commit || metadata.arch !== arch ||
        metadata.sha256 !== sha256 || metadata.archive !== archive || metadata.executableVersion !== version) {
      throw new Error(`Build metadata does not match ${archive} and the selected source commit.`);
    }
    validateArchive(bytes, arch);
    files.push({ name: archive, bytes, type: "application/gzip" });
    checksums += line;
  }
  files.push({ name: "SHA256SUMS", bytes: Buffer.from(checksums), type: "text/plain; charset=utf-8" });
  return files;
}

function compareVersions(a, b) {
  const left = a.split(".").map(BigInt);
  const right = b.split(".").map(BigInt);
  for (let index = 0; index < 3; index++) {
    if (left[index] !== right[index]) return left[index] > right[index] ? 1 : -1;
  }
  return 0;
}

export async function publishRelease({ version, files, store, publicRead, log = console.log }) {
  const latest = await store.read("latest.txt");
  if (latest !== null) {
    const value = latest.toString("utf8");
    if (!value.endsWith("\n") || !versionPattern.test(value.slice(0, -1))) throw new Error("Existing latest.txt is malformed.");
    if (compareVersions(value.slice(0, -1), version) > 0) throw new Error("Refusing to move latest.txt to an older release.");
  }
  // Check all existing objects before writing any. A retry can fill a partial
  // upload, but it can never replace a version with different bytes.
  const objects = files.map((file) => ({ ...file, key: `releases/v${version}/${file.name}` }));
  const missing = [];
  for (const object of objects) {
    const existing = await store.read(object.key);
    if (existing === null) missing.push(object);
    else if (!existing.equals(object.bytes)) throw new Error(`Refusing to overwrite changed release object: ${object.key}`);
  }
  for (const object of missing) {
    await store.write(object.key, object.bytes, { type: object.type, immutable: true });
    log(`Uploaded ${object.key}`);
  }
  for (const object of objects) {
    const publicBytes = await publicRead(object.key);
    if (digest(publicBytes) !== digest(object.bytes)) throw new Error(`Public download verification failed: ${object.key}`);
  }
  await store.write("latest.txt", Buffer.from(`${version}\n`), { type: "text/plain; charset=utf-8", immutable: false });
  if (!(await publicRead("latest.txt")).equals(Buffer.from(`${version}\n`))) throw new Error("Release uploaded, but the public latest.txt is stale. Check Cloudflare caching before retrying.");
  log(`Published Crow ${version}.`);
}

export async function createR2Store(environment = process.env) {
  const account = environment.CLOUDFLARE_ACCOUNT_ID;
  const bucket = environment.CROW_R2_BUCKET || "crow-releases";
  if (!/^[a-f0-9]{32}$/.test(account || "") || !/^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/.test(bucket)) throw new Error("Invalid Cloudflare account ID or R2 bucket name.");
  if (!environment.AWS_ACCESS_KEY_ID || !environment.AWS_SECRET_ACCESS_KEY) throw new Error("R2 S3 credentials are missing.");
  const { stdout, stderr } = await execute("aws", ["--version"]).catch(() => {
    throw new Error("Publication requires the official AWS CLI v2 on PATH.");
  });
  if (!`${stdout}${stderr}`.startsWith("aws-cli/2.")) throw new Error("Publication requires the official AWS CLI v2.");
  const temporary = await mkdtemp(join(tmpdir(), "crow-publish-"));
  let sequence = 0;
  const command = (args) => execute("aws", ["s3api", ...args, "--bucket", bucket, "--endpoint-url", `https://${account}.r2.cloudflarestorage.com`, "--region", "auto", "--no-cli-pager"], {
    env: { ...environment, AWS_EC2_METADATA_DISABLED: "true", AWS_PAGER: "",
      AWS_REQUEST_CHECKSUM_CALCULATION: "when_required", AWS_RESPONSE_CHECKSUM_VALIDATION: "when_required" },
    timeout: 180000,
    maxBuffer: 1024 * 1024,
  });
  return {
    async read(key) {
      const output = join(temporary, `get-${sequence++}`);
      try {
        await command(["get-object", "--key", key, output]);
        return await readFile(output);
      } catch (error) {
        if (/An error occurred \((NoSuchKey|404)\) when calling the GetObject operation/.test(error.stderr || "")) return null;
        throw error;
      } finally { await rm(output, { force: true }); }
    },
    async write(key, bytes, { type, immutable }) {
      const input = join(temporary, `put-${sequence++}`);
      await writeFile(input, bytes, { mode: 0o600 });
      try {
        await command(["put-object", "--key", key, "--body", input, "--content-type", type,
          "--cache-control", immutable ? "public, max-age=31536000, immutable" : "no-store, max-age=0",
          ...(immutable ? ["--if-none-match", "*"] : [])]);
      } finally { await rm(input, { force: true }); }
    },
    close: () => rm(temporary, { recursive: true, force: true }),
  };
}

async function readPublic(key) {
  const url = new URL(key, "https://downloads.birdapp.dev/");
  // Versioned objects are immutable. latest.txt must also be served fresh at
  // its normal URL, which is what installers and running Crow workers use.
  let failure;
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      const response = await fetch(url, { redirect: "error", signal: AbortSignal.timeout(60000), cache: "no-store" });
      if (!response.ok) throw new Error(`Public download returned HTTP ${response.status}: ${url}`);
      const chunks = [];
      let size = 0;
      for await (const chunk of response.body) {
        size += chunk.length;
        if (size > maximumArchive) throw new Error("Public download exceeds the size limit.");
        chunks.push(chunk);
      }
      return Buffer.concat(chunks);
    } catch (error) {
      failure = error;
      if (attempt < 3) await new Promise((resolve) => setTimeout(resolve, 3000));
    }
  }
  throw failure;
}

async function main() {
  const request = { version: process.env.RELEASE_VERSION, commit: process.env.BUILD_COMMIT, runId: process.env.BUILD_RUN_ID, confirmation: process.env.PUBLICATION_CONFIRMATION };
  validateRequest(request);
  if (process.argv[2] === "verify-run") {
    const repository = process.env.GITHUB_REPOSITORY;
    if (!/^[\w.-]+\/[\w.-]+$/.test(repository || "")) throw new Error("Invalid GitHub repository.");
    const response = await fetch(`https://api.github.com/repos/${repository}/actions/runs/${request.runId}`, {
      headers: { Authorization: `Bearer ${process.env.GH_TOKEN}`, Accept: "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28" },
      signal: AbortSignal.timeout(30000), redirect: "error",
    });
    if (!response.ok) throw new Error(`Cannot verify build run: HTTP ${response.status}`);
    validateBuildRun(await response.json(), { ...request, repository });
    console.log(`Verified build ${request.runId} at ${request.commit}.`);
  } else if (process.argv[2] === "publish" && process.argv.length === 4) {
    const files = await assembleRelease(resolve(process.argv[3]), request);
    const store = await createR2Store();
    try { await publishRelease({ ...request, files, store, publicRead: readPublic }); }
    finally { await store.close(); }
  } else throw new Error("Usage: node scripts/publish-hosted.mjs verify-run | publish ARTIFACTS_DIRECTORY");
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => { console.error(error.message); process.exitCode = 1; });
}
