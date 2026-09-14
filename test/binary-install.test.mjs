import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  writeFile,
  readFile,
  readlink,
  readdir,
  rm,
  symlink,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { gzipSync } from "node:zlib";
import {
  binaryPath,
  installBinary,
  prepareBinaryUpdate,
  checkBinaryUpdate,
} from "../dist/lib/binary-install.mjs";

async function fixture(t) {
  const dir = await mkdtemp(join(tmpdir(), "crow-binary-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return { dir, root: join(dir, "installation"), binDir: join(dir, "bin") };
}
function executable(arch = "x64") {
  const data = Buffer.alloc(64);
  data.set([0x7f, 0x45, 0x4c, 0x46, 2, 1]);
  data.writeUInt16LE(arch === "x64" ? 62 : 183, 18);
  return data;
}
function archive(entries = [{ name: "crow", body: executable() }]) {
  const blocks = [];
  for (const { name, body = Buffer.from("license"), kind = "0" } of entries) {
    const header = Buffer.alloc(512);
    header.write(name, 0);
    header.write("0000755\0", 100);
    header.write(`${body.length.toString(8).padStart(11, "0")}\0`, 124);
    header.fill(32, 148, 156);
    header.write(kind, 156);
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148);
    blocks.push(header, body, Buffer.alloc((512 - (body.length % 512)) % 512));
  }
  return gzipSync(Buffer.concat([...blocks, Buffer.alloc(1024)]));
}
function releaseOptions({
  payload = archive(),
  checksum = true,
  version = "0.3.0",
  returnedVersion = version,
  metadata = `${version}\n`,
  arch = "x64",
} = {}) {
  const calls = [];
  const name = `crow-v${version}-linux-${arch}.tar.gz`;
  const sums = `${checksum ? createHash("sha256").update(payload).digest("hex") : "0".repeat(64)}  ${name}\n`;
  return {
    calls,
    platform: "linux",
    arch,
    run: async (command, args) => {
      calls.push([command, args]);
      assert.notEqual(command, "gh");
      assert.deepEqual(args, ["--version"]);
      return { stdout: `crow ${returnedVersion}\n`, stderr: "" };
    },
    fetch: async (url, options) => {
      assert.equal(options.headers.Authorization, undefined);
      assert.equal(options.redirect, "error");
      if (url === "https://downloads.birdapp.dev/latest.txt")
        return new Response(metadata);
      const base = `https://downloads.birdapp.dev/releases/v${version}`;
      if (url === `${base}/SHA256SUMS`) return new Response(sums);
      assert.equal(url, `${base}/${name}`);
      return new Response(payload);
    },
  };
}

test("binary install copies into a durable release and keeps stable CLI symlinks", async (t) => {
  const { dir, root, binDir } = await fixture(t),
    source = join(dir, "download");
  await writeFile(source, executable());
  assert.equal(
    await installBinary(root, { executable: source, version: "0.2.0", binDir }),
    binaryPath(root),
  );
  await rm(source);
  assert.deepEqual(await readFile(binaryPath(root)), executable());
  assert.equal(await readlink(join(binDir, "crow")), binaryPath(root));
  const original = await readlink(join(root, "current"));
  await installBinary(root, {
    executable: binaryPath(root),
    version: "0.2.0",
    binDir,
  });
  assert.equal(await readlink(join(root, "current")), original);
});

test("install refuses unrelated executables and non-symlink current directories", async (t) => {
  const { dir, root, binDir } = await fixture(t),
    source = join(dir, "download");
  await writeFile(source, executable());
  await mkdir(binDir);
  await writeFile(join(binDir, "crow"), "unrelated");
  await assert.rejects(
    installBinary(root, { executable: source, version: "0.2.0", binDir }),
    /Refusing/,
  );
  assert.equal(await readFile(join(binDir, "crow"), "utf8"), "unrelated");
  await rm(join(binDir, "crow"));
  await symlink(source, join(binDir, "crow"));
  await assert.rejects(
    installBinary(root, { executable: source, version: "0.2.0", binDir }),
    /unrelated/,
  );
  await rm(join(binDir, "crow"));
  await mkdir(join(root, "current"), { recursive: true });
  await assert.rejects(
    installBinary(root, { executable: source, version: "0.2.0", binDir }),
    /non-symlink/,
  );
});

test("update validates before activation and can roll back without changing a custom binDir", async (t) => {
  const { dir, root, binDir } = await fixture(t),
    source = join(dir, "download");
  await writeFile(source, executable());
  await installBinary(root, { executable: source, version: "0.2.0", binDir });
  const previous = await readlink(join(root, "current")),
    options = releaseOptions();
  const prepared = await prepareBinaryUpdate(root, "0.2.0", options);
  assert.equal(prepared.version, "0.3.0");
  assert.equal(await readlink(join(root, "current")), previous);
  assert.equal(options.calls.length, 1);
  const rollback = await prepared.activate();
  assert.notEqual(await readlink(join(root, "current")), previous);
  assert.equal(await readlink(join(binDir, "crow")), binaryPath(root));
  await rollback();
  assert.equal(await readlink(join(root, "current")), previous);
  assert.equal(
    (await readdir(root)).some((name) => name.startsWith(".update-")),
    false,
  );
});

test("public hosted releases need no GitHub credentials and support arm64", async (t) => {
  const { root } = await fixture(t);
  const options = releaseOptions({
    arch: "arm64",
    payload: archive([{ name: "crow", body: executable("arm64") }]),
  });
  const prepared = await prepareBinaryUpdate(root, "0.2.0", options);
  assert.equal(prepared.version, "0.3.0");
});

test("invalid release payloads are rejected before executable validation or activation", async (t) => {
  const { root } = await fixture(t);
  for (const input of [
    { checksum: false },
    { payload: archive([{ name: "../crow", body: executable() }]) },
    { payload: archive([{ name: "crow", body: executable(), kind: "2" }]) },
    {
      payload: archive([
        { name: "crow", body: executable() },
        { name: "crow", body: executable() },
      ]),
    },
    { payload: archive([{ name: "crow", body: executable("arm64") }]) },
    { payload: archive([{ name: "LICENSE" }]) },
  ]) {
    const options = releaseOptions(input);
    await assert.rejects(prepareBinaryUpdate(root, "0.2.0", options));
    assert.equal(options.calls.length, 0);
    assert.equal((await readdir(root)).length, 0);
  }
  await assert.rejects(
    prepareBinaryUpdate(
      root,
      "0.2.0",
      releaseOptions({ returnedVersion: "0.2.9" }),
    ),
    /unexpected version/,
  );
});

test("update checks use semantic versions and return warnings for unavailable hosted releases", async (t) => {
  const { root } = await fixture(t);
  assert.deepEqual(await checkBinaryUpdate("0.3.0", releaseOptions()), {
    available: false,
    version: "0.3.0",
  });
  assert.deepEqual(
    await checkBinaryUpdate("0.3.0", releaseOptions({ version: "0.10.0" })),
    { available: true, version: "0.10.0" },
  );
  assert.equal(
    await prepareBinaryUpdate(root, "0.3.0", releaseOptions()),
    null,
  );
  const unavailable = await checkBinaryUpdate("0.2.0", {
    fetch: async () => new Response("", { status: 404 }),
  });
  assert.equal(unavailable.available, false);
  assert.match(unavailable.warning, /Crow release download failed \(404\)/);
  await assert.rejects(
    prepareBinaryUpdate(root, "0.2.0", { platform: "darwin" }),
    /Linux/,
  );
  assert.match(
    (await checkBinaryUpdate("0.2.0", releaseOptions({ version: "../bad" })))
      .warning,
    /Invalid/,
  );
});

test("latest metadata accepts only a stable version and one optional newline", async (t) => {
  const { root } = await fixture(t);
  for (const metadata of ["0.3.0", "0.3.0\n"])
    assert.deepEqual(
      await checkBinaryUpdate("0.2.0", releaseOptions({ metadata })),
      { available: true, version: "0.3.0" },
    );
  for (const metadata of [
    "",
    "v0.3.0",
    "0.3.0-rc.1",
    "0.3.0+build",
    "00.3.0",
    " 0.3.0",
    "0.3.0 ",
    "0.3.0\r\n",
    "0.3.0\n\n",
    "0.3.0\n0.4.0",
    "0.3.0\n/path",
    "9007199254740992.0.0",
    "0".repeat(129),
  ]) {
    const options = releaseOptions({ metadata });
    const status = await checkBinaryUpdate("0.2.0", options);
    assert.equal(status.available, false, JSON.stringify(metadata));
    assert.ok(status.warning);
    await assert.rejects(prepareBinaryUpdate(root, "0.2.0", options));
    assert.equal(options.calls.length, 0);
  }
  await assert.rejects(readdir(root), { code: "ENOENT" });
});

test("download failures and oversized checksums never execute a candidate", async (t) => {
  const { root } = await fixture(t);
  for (const failure of ["network", "status", "size"]) {
    const options = releaseOptions();
    const download = options.fetch;
    options.fetch = async (url, request) => {
      if (!url.endsWith("/SHA256SUMS")) return download(url, request);
      if (failure === "network") throw new Error("Connection lost");
      if (failure === "status") return new Response("", { status: 503 });
      return new Response("x".repeat(1024 * 1024 + 1));
    };
    await assert.rejects(
      prepareBinaryUpdate(root, "0.2.0", options),
      /Connection lost|download failed|size limit/,
    );
    assert.equal(options.calls.length, 0);
    assert.deepEqual(await readdir(root), []);
  }
});

test("source wrapper replacement requires explicit migration and recognizable Crow content", async (t) => {
  const { dir, root, binDir } = await fixture(t),
    source = join(dir, "download");
  await writeFile(source, executable());
  await mkdir(binDir);
  const wrapper =
    '#!/usr/bin/env bash\nset -euo pipefail\nexport PATH=/usr/bin:"$PATH"\nexec /usr/bin/node /old-crow/current/bin/crow.mjs "$@"\n';
  await writeFile(join(binDir, "crow"), wrapper);
  await assert.rejects(
    installBinary(root, { executable: source, version: "0.2.0", binDir }),
    /Refusing/,
  );
  await installBinary(root, {
    executable: source,
    version: "0.2.0",
    binDir,
    allowSourceMigration: true,
  });
  assert.equal(await readlink(join(binDir, "crow")), binaryPath(root));
});

test("rollback refuses to overwrite a subsequently activated release", async (t) => {
  const { root } = await fixture(t);
  const first = await prepareBinaryUpdate(root, "0.2.0", releaseOptions());
  const rollback = await first.activate();
  const second = await prepareBinaryUpdate(
    root,
    "0.3.0",
    releaseOptions({ version: "0.4.0" }),
  );
  await second.activate();
  await assert.rejects(rollback(), /changed again/);
  assert.equal(
    await readlink(join(root, "current")),
    second.executable.slice(0, -5),
  );
});
