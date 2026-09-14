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
  authenticated = true,
  arch = "x64",
} = {}) {
  const calls = [];
  const metadata = JSON.stringify({
    tag_name: `v${version}`,
    draft: false,
    prerelease: false,
  });
  const name = `crow-v${version}-linux-${arch}.tar.gz`;
  const sums = `${checksum ? createHash("sha256").update(payload).digest("hex") : "0".repeat(64)}  ${name}\n`;
  return {
    calls,
    platform: "linux",
    arch,
    run: async (command, args) => {
      calls.push([command, args]);
      if (command === "gh" && args[0] === "api") {
        if (!authenticated) throw new Error("gh unavailable");
        return { stdout: metadata, stderr: "" };
      }
      if (command === "gh") {
        const dir = args[args.indexOf("--dir") + 1];
        await writeFile(join(dir, name), payload);
        await writeFile(join(dir, "SHA256SUMS"), sums);
        return { stdout: "", stderr: "" };
      }
      assert.deepEqual(args, ["--version"]);
      return { stdout: `crow ${returnedVersion}\n`, stderr: "" };
    },
    fetch: async (url, options) => {
      assert.equal(options.headers.Authorization, undefined);
      if (url.endsWith("/latest")) return new Response(metadata);
      if (url.endsWith("/SHA256SUMS")) return new Response(sums);
      assert.ok(url.endsWith(`/${name}`));
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
  assert.equal(options.calls.filter(([command]) => command !== "gh").length, 1);
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

test("public release fallback never needs GitHub credentials and supports arm64", async (t) => {
  const { root } = await fixture(t);
  const options = releaseOptions({
    authenticated: false,
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
    assert.equal(
      options.calls.every(([command]) => command === "gh"),
      true,
    );
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

test("update checks use semantic versions and return warnings for unavailable private releases", async (t) => {
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
    run: async () => {
      throw Error("missing gh");
    },
    fetch: async () => new Response("", { status: 404 }),
  });
  assert.equal(unavailable.available, false);
  assert.match(unavailable.warning, /gh auth login/);
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
