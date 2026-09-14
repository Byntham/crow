import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  readFile,
  readdir,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { installCodex } from "../dist/lib/codex-install.mjs";
import { processRun } from "../dist/lib/util.mjs";

async function fixture(t, arch = "x64") {
  const root = await mkdtemp(join(tmpdir(), "crow-codex-install-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const target = `${arch === "x64" ? "x86_64" : "aarch64"}-unknown-linux-musl`;
  const packageRoot = join(root, "source");
  const files = {
    "bin/codex": "fixture codex executable\n",
    "bin/codex-code-mode-host": "fixture Code Mode executable\n",
    "codex-path/rg": "fixture rg\n",
    "codex-resources/bwrap": "fixture bwrap\n",
    "codex-resources/zsh/bin/zsh": "fixture zsh\n",
    "codex-package.json": JSON.stringify({
      layoutVersion: 1,
      version: "0.154.0",
      target,
      variant: "codex",
      entrypoint: "bin/codex",
      resourcesDir: "codex-resources",
      pathDir: "codex-path",
    }),
  };
  for (const [name, content] of Object.entries(files)) {
    await mkdir(dirname(join(packageRoot, name)), { recursive: true });
    await writeFile(join(packageRoot, name), content);
  }
  const archive = join(root, "fixture.tar.gz");
  await processRun("tar", [
    "-czf",
    archive,
    "-C",
    packageRoot,
    ...Object.keys(files),
  ]);
  const bytes = await readFile(archive);
  const name = `codex-package-${target}.tar.gz`;
  const asset = {
    name,
    size: bytes.length,
    digest: `sha256:${createHash("sha256").update(bytes).digest("hex")}`,
    browser_download_url: `https://github.com/openai/codex/releases/download/rust-v0.154.0/${name}`,
  };
  const release = {
    tag_name: "rust-v0.154.0",
    draft: false,
    prerelease: false,
    assets: [asset],
  };
  const calls = [];
  const fetcher = async (url) => {
    calls.push(url);
    if (url === "https://api.github.com/repos/openai/codex/releases/latest")
      return Response.json(release);
    assert.equal(url, asset.browser_download_url);
    return new Response(bytes);
  };
  const commands = [];
  const run = async (command, args, options) => {
    commands.push({ command, args });
    if (command.endsWith("/bin/codex")) {
      assert.deepEqual(args, ["--version"]);
      return { stdout: "codex-cli 0.154.0\n", stderr: "" };
    }
    assert.equal(command, "tar");
    return processRun(command, args, options);
  };
  return {
    root: join(root, "crow state"),
    asset,
    release,
    fetcher,
    calls,
    run,
    commands,
    bytes,
    files,
    arch,
  };
}

for (const arch of ["x64", "arm64"]) {
  test(`Codex installer verifies and installs the complete official ${arch} standalone package without npm`, async (t) => {
    const f = await fixture(t, arch);
    const binary = await installCodex(f.root, {
      fetcher: f.fetcher,
      run: f.run,
      platform: "linux",
      arch,
    });
    assert.match(binary, /\/tools\/codex-0\.154\.0-.*\/bin\/codex$/);
    const installed = dirname(dirname(binary));
    for (const [name, expected] of Object.entries(f.files)) {
      assert.equal(await readFile(join(installed, name), "utf8"), expected);
      assert.equal(
        (await stat(join(installed, name))).mode & 0o777,
        name === "codex-package.json" ? 0o600 : 0o700,
      );
    }
    assert.equal(f.calls.length, 2);
    assert.equal((await readdir(join(f.root, "tools"))).length, 1);
    assert.equal(
      f.commands.some(({ command }) => /npm|node/.test(command)),
      false,
    );
  });
}

test("Codex installer rejects missing digest and nonofficial download URLs before downloading", async (t) => {
  for (const mutation of [
    (asset) => {
      delete asset.digest;
    },
    (asset) => {
      asset.browser_download_url = "https://example.com/codex.tar.gz";
    },
    (asset) => {
      asset.size = 1024 ** 4;
    },
  ]) {
    const f = await fixture(t);
    mutation(f.asset);
    await assert.rejects(installCodex(f.root, f), /published SHA-256 digest/);
    assert.equal(f.calls.length, 1);
    assert.equal(f.commands.length, 0);
  }
});

test("Codex installer rejects corrupted downloads before any tar command and removes staging", async (t) => {
  const f = await fixture(t);
  f.asset.digest = `sha256:${"0".repeat(64)}`;
  await assert.rejects(installCodex(f.root, f), /SHA-256 checksum/);
  assert.equal(f.commands.length, 0);
  assert.deepEqual(await readdir(join(f.root, "tools")), []);
});

test("Codex installer rejects links, traversal, duplicates, and missing helpers before extraction", async (t) => {
  for (const [names, entries] of [
    ["bin/codex\n", "lrwxrwxrwx codex -> /tmp/target\n"],
    ["../escaped\n", "-rwxrwxrwx ../escaped\n"],
    ["bin/codex\nbin/codex\n", "-rwxrwxrwx bin/codex\n-rwxrwxrwx bin/codex\n"],
    ["bin/codex\n", "-rwxrwxrwx bin/codex\n"],
  ]) {
    const f = await fixture(t);
    const run = async (command, args) => {
      assert.equal(command, "tar");
      assert.ok(
        ["-tzf", "-tvzf"].includes(args[0]),
        "must never extract unsafe archives",
      );
      return { stdout: args[0] === "-tzf" ? names : entries, stderr: "" };
    };
    await assert.rejects(
      installCodex(f.root, { ...f, run }),
      /unsafe Codex archive entry|missing bin\/codex-code-mode-host/,
    );
    assert.deepEqual(await readdir(join(f.root, "tools")), []);
  }
});

test("Codex installer rejects an unexpected executable version without activating files", async (t) => {
  const f = await fixture(t);
  const run = async (command, args, options) => {
    if (command.endsWith("/bin/codex"))
      return { stdout: "codex-cli 0.1.0", stderr: "" };
    return f.run(command, args, options);
  };
  await assert.rejects(
    installCodex(f.root, { ...f, run }),
    /unexpected version/,
  );
  assert.deepEqual(await readdir(join(f.root, "tools")), []);
});

test("Codex installer refuses unsupported platforms without a network request", async () => {
  await assert.rejects(
    installCodex("/unused", {
      platform: "darwin",
      arch: "x64",
      fetcher: async () => assert.fail("must not fetch"),
    }),
    /supports Linux x64 and arm64/,
  );
});
