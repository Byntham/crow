import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { gzipSync } from "node:zlib";
import { assembleRelease, publishRelease, validateArchive, validateBuildRun, validateRequest } from "../scripts/publish-hosted.mjs";

const commit = "a".repeat(40);
const request = { version: "0.2.0", commit, runId: "123", confirmation: "publish 0.2.0" };
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");

function archive(arch = "x64") {
  const binary = Buffer.alloc(64);
  binary.set([127, 69, 76, 70, 2, 1]);
  binary.writeUInt16LE(arch === "x64" ? 62 : 183, 18);
  const parts = [];
  for (const [name, bytes] of [["crow", binary], ["THIRD_PARTY_NOTICES", Buffer.from("License")]]) {
    const header = Buffer.alloc(512);
    header.write(name);
    header.write(bytes.length.toString(8).padStart(11, "0") + "\0", 124);
    header.fill(32, 148, 156);
    header[156] = 48;
    header.write(header.reduce((sum, byte) => sum + byte, 0).toString(8).padStart(6, "0") + "\0 ", 148);
    parts.push(header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
  }
  parts.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(parts));
}

test("publication requires explicit stable version and a successful exact build from this repository", () => {
  validateRequest(request);
  for (const change of [{ confirmation: "yes" }, { version: "0.2.0-beta" }, { version: "00.2.0" }, { commit: "main" }, { runId: "123/other" }]) {
    assert.throws(() => validateRequest({ ...request, ...change }));
  }
  const repository = "Byntham/Crow";
  const run = { id: 123, repository: { full_name: repository }, head_repository: { full_name: repository }, head_sha: commit, path: ".github/workflows/release.yml", status: "completed", conclusion: "success", event: "workflow_dispatch" };
  validateBuildRun(run, { ...request, repository });
  for (const change of [{ id: 124 }, { head_sha: "b".repeat(40) }, { path: ".github/workflows/other.yml" }, { conclusion: "failure" }, { event: "pull_request" }, { head_repository: { full_name: "fork/Crow" } }]) {
    assert.throws(() => validateBuildRun({ ...run, ...change }, { ...request, repository }));
  }
});

test("release archive validation rejects wrong architecture and corrupted tar headers", () => {
  validateArchive(archive("x64"), "x64");
  validateArchive(archive("arm64"), "arm64");
  assert.throws(() => validateArchive(archive("arm64"), "x64"), /ELF64/);
  assert.throws(() => validateArchive(gzipSync(Buffer.alloc(1024)), "x64"), /Incomplete/);
  assert.throws(() => validateArchive(gzipSync(Buffer.alloc(2048, 1)), "x64"), /Invalid tar/);
});

test("assembly verifies both checksums and source/version metadata before publishing", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "crow-release-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  for (const arch of ["x64", "arm64"]) {
    const path = join(directory, `crow-linux-${arch}`);
    await mkdir(path);
    const name = `crow-v${request.version}-linux-${arch}.tar.gz`;
    const bytes = archive(arch);
    await writeFile(join(path, name), bytes);
    await writeFile(join(path, "SHA256SUMS"), `${hash(bytes)}  ${name}\n`);
    await writeFile(join(path, "build-metadata.json"), JSON.stringify({ version: request.version, executableVersion: request.version, commit, arch, archive: name, sha256: hash(bytes) }));
  }
  const result = await assembleRelease(directory, request);
  assert.equal(result.length, 3);
  assert.equal(result.at(-1).name, "SHA256SUMS");
  await assert.rejects(assembleRelease(directory, { ...request, commit: "b".repeat(40) }), /metadata/);
  await writeFile(join(directory, "crow-linux-x64", "SHA256SUMS"), "incorrect\n");
  await assert.rejects(assembleRelease(directory, request), /Checksum/);
});

function fixture(initial = {}) {
  const data = new Map(Object.entries(initial).map(([key, value]) => [key, Buffer.from(value)]));
  const events = [];
  return {
    data, events,
    store: {
      read: async (key) => data.get(key) ?? null,
      write: async (key, bytes, options) => {
        events.push(`write:${key}`);
        if (options.immutable) assert.equal(data.has(key), false);
        data.set(key, bytes);
      },
    },
    publicRead: async (key) => { events.push(`verify:${key}`); return data.get(key); },
    log: () => {},
  };
}

const files = [
  { name: "crow-v0.2.0-linux-x64.tar.gz", bytes: Buffer.from("x64"), type: "application/gzip" },
  { name: "crow-v0.2.0-linux-arm64.tar.gz", bytes: Buffer.from("arm64"), type: "application/gzip" },
  { name: "SHA256SUMS", bytes: Buffer.from("checksums"), type: "text/plain" },
];

test("publication makes latest visible only after every public file is verified and retries are idempotent", async () => {
  const state = fixture();
  await publishRelease({ version: request.version, files, ...state });
  assert.equal(state.data.get("latest.txt").toString(), "0.2.0\n");
  assert.deepEqual(state.events.slice(-5), [
    "verify:releases/v0.2.0/crow-v0.2.0-linux-x64.tar.gz",
    "verify:releases/v0.2.0/crow-v0.2.0-linux-arm64.tar.gz",
    "verify:releases/v0.2.0/SHA256SUMS", "write:latest.txt", "verify:latest.txt",
  ]);
  state.events.length = 0;
  await publishRelease({ version: request.version, files, ...state });
  assert.deepEqual(state.events.filter((entry) => entry.startsWith("write:")), ["write:latest.txt"]);
});

test("changed versions, downgrades, and failed public verification never change latest", async () => {
  const changed = fixture({ "releases/v0.2.0/SHA256SUMS": "old" });
  await assert.rejects(publishRelease({ version: request.version, files, ...changed }), /overwrite changed/);
  assert.deepEqual(changed.events, []);
  const newer = fixture({ "latest.txt": "0.3.0\n" });
  await assert.rejects(publishRelease({ version: request.version, files, ...newer }), /older release/);
  assert.deepEqual(newer.events, []);
  const corrupt = fixture({ "latest.txt": "0.1.0\n" });
  await assert.rejects(publishRelease({ version: request.version, files, ...corrupt, publicRead: async () => Buffer.from("wrong") }), /verification failed/);
  assert.equal(corrupt.data.get("latest.txt").toString(), "0.1.0\n");
});

test("an interrupted upload can resume without overwriting complete objects", async () => {
  const state = fixture({ "releases/v0.2.0/crow-v0.2.0-linux-x64.tar.gz": "x64" });
  await publishRelease({ version: request.version, files, ...state });
  assert.equal(state.events.includes("write:releases/v0.2.0/crow-v0.2.0-linux-x64.tar.gz"), false);
  assert.equal(state.data.get("latest.txt").toString(), "0.2.0\n");
});
