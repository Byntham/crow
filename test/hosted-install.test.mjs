import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
  symlinkSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const installer = fileURLToPath(new URL("../website/install.sh", import.meta.url));

function fixture(t, options = {}) {
  const root = mkdtempSync(join(tmpdir(), "crow-hosted-install-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const name of ["bin", "assets", "stage", "temporary", "home"])
    mkdirSync(join(root, name));
  const version = "0.2.0";
  const architecture = options.architecture === "aarch64" ? "arm64" : "x64";
  const archiveName = `crow-v${version}-linux-${architecture}.tar.gz`;
  const asset = join(root, "assets", archiveName);
  const command = (name, source) => writeFileSync(join(root, "bin", name), source, { mode: 0o755 });
  command("uname", `#!/bin/sh\ncase "$1" in -s) echo '${options.os ?? "Linux"}';; -m) echo '${options.architecture ?? "x86_64"}';; esac\n`);
  command("getconf", `#!/bin/sh\n${options.musl ? "exit 1" : "echo 'glibc 2.39'"}\n`);
  command("curl", `#!${process.execPath}
import { copyFileSync, appendFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const args = process.argv.slice(2);
appendFileSync(process.env.FIXTURE_LOG, JSON.stringify(args) + '\\n');
const url = args.at(-1);
const output = args[args.indexOf('--output') + 1];
if (process.env.FIXTURE_FAIL && url.endsWith(process.env.FIXTURE_FAIL)) {
  writeFileSync(output, 'interrupted'); process.exit(22);
}
if (!url.startsWith('https://downloads.birdapp.dev/')) process.exit(3);
copyFileSync(join(process.env.FIXTURE_ASSETS, url.split('/').at(-1)), output);
`);
  const executable = join(root, "stage", "crow");
  writeFileSync(executable, `#!/bin/sh
if [ "$1" = --version ]; then echo '${options.binaryVersion ?? version}'; exit 0; fi
printf '%s\\n' "$*" >> "$FIXTURE_INVOKED"
if [ "\${FIXTURE_EXPECT_TTY:-}" = true ]; then
  [ -t 0 ] || exit 11
  exit 0
fi
if read -r unexpected; then echo 'Unexpected stdin' >&2; exit 10; fi
`, { mode: 0o755 });
  writeFileSync(join(root, "stage", "THIRD_PARTY_NOTICES"), "Fixture notice\n");
  let entries = ["crow", "THIRD_PARTY_NOTICES"];
  if (options.extraFile) {
    writeFileSync(join(root, "stage", "extra"), "unexpected");
    entries.push("extra");
  }
  if (options.symlink) {
    rmSync(executable);
    symlinkSync("/bin/sh", executable);
  }
  if (options.duplicate) entries.push("crow");
  const tar = spawnSync("tar", ["-czf", asset, "-C", join(root, "stage"), ...entries]);
  assert.equal(tar.status, 0, tar.stderr.toString());
  const digest = options.corrupt ? "0".repeat(64) : createHash("sha256").update(readFileSync(asset)).digest("hex");
  const checksum = `${digest}  ${archiveName}\n`;
  writeFileSync(join(root, "assets", "SHA256SUMS"), options.duplicateChecksum ? checksum.repeat(2) : checksum);
  writeFileSync(join(root, "assets", "latest.txt"), options.latest ?? `${version}\n`);
  const env = {
    ...process.env,
    PATH: `${join(root, "bin")}:/usr/bin:/bin`,
    HOME: join(root, "home"),
    TMPDIR: join(root, "temporary"),
    FIXTURE_ASSETS: join(root, "assets"),
    FIXTURE_LOG: join(root, "requests"),
    FIXTURE_INVOKED: join(root, "invoked"),
    FIXTURE_FAIL: options.fail ?? "",
    FIXTURE_EXPECT_TTY: options.expectTTY ? "true" : "",
  };
  const read = (name) => {
    try { return readFileSync(join(root, name), "utf8"); }
    catch (error) { if (error.code === "ENOENT") return ""; throw error; }
  };
  return {
    root,
    read,
    env,
    run: (args = [], input) => spawnSync("/bin/sh", input === undefined ? [installer, ...args] : ["-s", "--", ...args], {
      env, detached: true, input, encoding: "utf8", timeout: 10000,
    }),
    clean: () => assert.deepEqual(readdirSync(join(root, "temporary")), []),
  };
}

test("hosted installer verifies public assets and installs without setup when no terminal is present", (t) => {
  const f = fixture(t);
  const result = f.run([], readFileSync(installer, "utf8"));
  assert.equal(result.status, 0, result.stderr);
  assert.equal(f.read("invoked"), "install --no-setup\n");
  assert.match(result.stdout, /No terminal detected/);
  const requests = f.read("requests").trim().split("\n").map(JSON.parse);
  assert.deepEqual(requests.map((args) => args.at(-1)), [
    "https://downloads.birdapp.dev/latest.txt",
    "https://downloads.birdapp.dev/releases/v0.2.0/SHA256SUMS",
    "https://downloads.birdapp.dev/releases/v0.2.0/crow-v0.2.0-linux-x64.tar.gz",
  ]);
  for (const args of requests) {
    assert.equal(args[args.indexOf("--proto") + 1], "=https");
    assert.equal(args[args.indexOf("--proto-redir") + 1], "=https");
    assert.ok(args.includes("--fail"));
    assert.equal(args[args.indexOf("--retry") + 1], "2");
  }
  f.clean();
});

test("explicit version skips latest discovery and selects the ARM64 archive", (t) => {
  const f = fixture(t, { architecture: "aarch64" });
  const result = f.run(["--version", "0.2.0", "--no-setup"]);
  assert.equal(result.status, 0, result.stderr);
  assert.doesNotMatch(f.read("requests"), /latest.txt/);
  assert.match(f.read("requests"), /linux-arm64.tar.gz/);
  assert.equal(f.read("invoked"), "install --no-setup\n");
  f.clean();
});

test("piped installer reconnects setup to the controlling terminal", {
  skip: spawnSync("script", ["--version"]).status !== 0,
}, (t) => {
  const f = fixture(t, { expectTTY: true });
  const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
  const result = spawnSync("script", ["-q", "-e", "-c", `cat ${quote(installer)} | sh`, "/dev/null"], {
    env: f.env, encoding: "utf8", timeout: 10000, input: "",
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.equal(f.read("invoked"), "install\n");
  f.clean();
});

for (const [name, options, message] of [
  ["checksum mismatch", { corrupt: true }, /Checksum verification failed/],
  ["duplicate checksum", { duplicateChecksum: true }, /checksum is missing, duplicated, or invalid/],
  ["extra archive file", { extraFile: true }, /unexpected files/],
  ["archive symlink", { symlink: true }, /regular files only/],
  ["duplicate archive entry", { duplicate: true }, /unexpected files/],
  ["binary version mismatch", { binaryVersion: "0.1.0" }, /unexpected version/],
  ["interrupted download", { fail: ".tar.gz" }, /Download failed/],
  ["invalid latest response", { latest: "0.2.0\nmalicious\n" }, /Invalid latest-version/],
  ["prerelease latest response", { latest: "0.2.0-rc.1\n" }, /Invalid latest-version/],
  ["unsupported operating system", { os: "Darwin" }, /Linux only/],
  ["unsupported architecture", { architecture: "riscv64" }, /x64 and ARM64/],
  ["musl Linux", { musl: true }, /requires glibc/],
]) {
  test(`hosted installer rejects ${name} without installing and cleans temporary files`, (t) => {
    const f = fixture(t, options);
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, message);
    assert.equal(f.read("invoked"), "");
    if (options.os || options.architecture || options.musl) assert.equal(f.read("requests"), "");
    f.clean();
  });
}

test("invalid version arguments never reach the download host", (t) => {
  const f = fixture(t);
  for (const version of ["../../other", "0.2.0; touch owned", "01.2.0", "v0.2.0", "0.2.0\n", ""]) {
    const result = f.run(["--version", version]);
    assert.notEqual(result.status, 0);
    assert.equal(f.read("requests"), "");
    f.clean();
  }
});

test("a truncated installer does not start downloading or installing", (t) => {
  const f = fixture(t);
  const script = readFileSync(installer, "utf8");
  const result = f.run([], script.slice(0, script.lastIndexOf('crow_main "$@"')));
  assert.equal(result.status, 0, result.stderr);
  assert.equal(f.read("requests"), "");
  assert.equal(f.read("invoked"), "");
  f.clean();
});
