import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  rmSync,
  readFileSync,
  writeFileSync,
  mkdirSync,
  existsSync,
  readlinkSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
const project = fileURLToPath(new URL("..", import.meta.url));
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "crow-installer-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  return root;
}
function install(env) {
  return spawnSync("bash", ["scripts/install.sh"], {
    cwd: project,
    env,
    encoding: "utf8",
    timeout: 30000,
  });
}

test(
  "native installer preserves repeated releases and runs the installed CLI through paths with spaces",
  { skip: process.platform !== "linux" },
  (t) => {
    const root = fixture(t);
    const env = {
      ...process.env,
      CROW_NODE: process.execPath,
      CROW_INSTALL_DIR: join(root, "install with spaces"),
      CROW_BIN_DIR: join(root, "bin with spaces"),
    };
    const first = install(env);
    assert.equal(first.status, 0, first.stderr || first.stdout);
    const release = readlinkSync(join(env.CROW_INSTALL_DIR, "current"));
    const second = install(env);
    assert.equal(second.status, 0, second.stderr || second.stdout);
    assert.equal(readlinkSync(join(env.CROW_INSTALL_DIR, "current")), release);
    const help = spawnSync(join(env.CROW_BIN_DIR, "crow"), ["help"], {
      env,
      encoding: "utf8",
      timeout: 10000,
    });
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /crow setup/);
    const installedRoot = join(env.CROW_INSTALL_DIR, "current");
    assert.equal(existsSync(join(installedRoot, "lib/provider.mjs")), true);
    assert.equal(
      existsSync(join(installedRoot, "bin/inspection-mcp.mjs")),
      true,
    );
    assert.equal(existsSync(join(installedRoot, "lib/provider.mts")), false);
    assert.equal(existsSync(join(installedRoot, "node_modules")), false);
    assert.equal(existsSync(join(installedRoot, "dist")), false);
    assert.equal(
      JSON.parse(readFileSync(join(installedRoot, "package.json"), "utf8")).bin
        .crow,
      "bin/crow.mjs",
    );
    const metadata = JSON.parse(
      readFileSync(join(env.CROW_INSTALL_DIR, "current/install.json"), "utf8"),
    );
    assert.equal(metadata.bin, env.CROW_BIN_DIR);
    assert.equal(metadata.installation, env.CROW_INSTALL_DIR);
    assert.equal(metadata.source, project.replace(/\/$/, ""));
  },
);

test(
  "native installer bootstraps an official Node archive and rejects a corrupt checksum before activation",
  {
    skip:
      process.platform !== "linux" || !["x64", "arm64"].includes(process.arch),
  },
  (t) => {
    const root = fixture(t),
      bundle = join(root, "bundle"),
      mocks = join(root, "mocks");
    mkdirSync(join(bundle, "node-fixture/bin"), { recursive: true });
    mkdirSync(mocks);
    writeFileSync(
      join(bundle, "node-fixture/bin/node"),
      `#!/bin/sh\nif [ "$1" = -e ]; then exit 0; fi\nexec '${process.execPath.replaceAll("'", "'\\''")}' "$@"\n`,
      { mode: 0o755 },
    );
    const archive = `node-v24.21.0-linux-${process.arch}.tar.xz`;
    const tar = spawnSync(
      "tar",
      ["-cJf", join(root, archive), "-C", bundle, "node-fixture"],
      { encoding: "utf8" },
    );
    assert.equal(tar.status, 0, tar.stderr);
    const digest = createHash("sha256")
      .update(readFileSync(join(root, archive)))
      .digest("hex");
    writeFileSync(join(root, "SHASUMS256.txt"), `${digest}  ${archive}\n`);
    writeFileSync(join(mocks, "old-node"), "#!/bin/sh\nexit 1\n", {
      mode: 0o755,
    });
    // Local curl stand-in proves URL selection and checksum behavior without networking.
    writeFileSync(
      join(mocks, "curl"),
      `#!${process.execPath}\nconst fs=require('fs');const args=process.argv.slice(2);const url=args.find(x=>x.startsWith('https://'));if(!url?.startsWith('https://nodejs.org/dist/latest-v24.x/'))process.exit(5);const dest=args[args.indexOf('-o')+1];fs.copyFileSync(process.env.CROW_FIXTURE+'/'+url.split('/').at(-1),dest);\n`,
      { mode: 0o755 },
    );
    const env = {
      ...process.env,
      PATH: `${mocks}:${process.env.PATH}`,
      CROW_NODE: join(mocks, "old-node"),
      CROW_FIXTURE: root,
      CROW_INSTALL_DIR: join(root, "install"),
      CROW_BIN_DIR: join(root, "bin"),
    };
    const good = install(env);
    assert.equal(good.status, 0, good.stderr || good.stdout);
    assert.equal(existsSync(join(env.CROW_INSTALL_DIR, "node/bin/node")), true);
    writeFileSync(
      join(root, "SHASUMS256.txt"),
      `${"0".repeat(64)}  ${archive}\n`,
    );
    env.CROW_INSTALL_DIR = join(root, "bad-checksum");
    const bad = install(env);
    assert.notEqual(bad.status, 0);
    assert.equal(existsSync(join(env.CROW_INSTALL_DIR, "current")), false);
  },
);
