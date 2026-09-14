import test from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { once } from "node:events";
import { createServer } from "node:net";
import { defaults, save } from "../dist/lib/config.mjs";
import {
  installBinary,
  prepareBinaryUpdate,
} from "../dist/lib/binary-install.mjs";
import { processRun } from "../dist/lib/util.mjs";

const binary =
  process.env.CROW_TEST_BINARY && resolve(process.env.CROW_TEST_BINARY);
const options = { skip: !binary, timeout: 15000 };
test(
  "downloaded binary installs without a terminal and preserves existing onboarding",
  options,
  async (t) => {
    const { root, env } = await fixture(t);
    const run = () =>
      spawnSync(binary, ["install", "--no-setup"], {
        env,
        cwd: root,
        encoding: "utf8",
      });
    const installed = run();
    assert.equal(installed.status, 0, installed.stderr);
    assert.match(installed.stdout, /Start or continue setup with:/);
    const command = join(root, ".local/bin/crow");
    const version = spawnSync(command, ["--version"], {
      env,
      encoding: "utf8",
    });
    assert.equal(version.status, 0, version.stderr);
    await writeFile(join(root, "config.json"), "saved onboarding");
    const repeated = run();
    assert.equal(repeated.status, 0, repeated.stderr);
    assert.equal(
      await readFile(join(root, "config.json"), "utf8"),
      "saved onboarding",
    );
  },
);
test(
  "packaged archive survives update validation and installation",
  options,
  async (t) => {
    const { root, env } = await fixture(t);
    const pkg = JSON.parse(
      await readFile(new URL("../package.json", import.meta.url), "utf8"),
    );
    const archive = `crow-v${pkg.version}-linux-${process.arch}.tar.gz`;
    const releaseDirectory = new URL("../dist-release/", import.meta.url);
    const candidate = await prepareBinaryUpdate(root, "0.0.0", {
      run: (command, args, runOptions) =>
        processRun(command, args, { ...runOptions, env }),
      fetch: async (url) => {
        if (url === "https://downloads.birdapp.dev/latest.txt")
          return new Response(`${pkg.version}\n`);
        const name = new URL(url).pathname.split("/").at(-1);
        assert.ok([archive, "SHA256SUMS"].includes(name));
        assert.equal(
          url,
          `https://downloads.birdapp.dev/releases/v${pkg.version}/${name}`,
        );
        return new Response(await readFile(new URL(name, releaseDirectory)));
      },
    });
    const installed = await installBinary(root, {
      executable: candidate.executable,
      version: pkg.version,
      binDir: join(root, "bin"),
    });
    for (const executable of [installed, join(root, "bin/crow")]) {
      const result = spawnSync(executable, ["--version"], {
        env,
        cwd: root,
        encoding: "utf8",
      });
      assert.equal(result.status, 0, result.stderr);
      assert.match(
        result.stdout,
        new RegExp(pkg.version.replaceAll(".", "\\.")),
      );
    }
  },
);
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-binary-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  // No Node, package manager, or source checkout can be found by the binary.
  return {
    root,
    env: {
      PATH: "/nonexistent",
      HOME: root,
      CROW_HOME: root,
      NODE_OPTIONS: "--this-must-be-ignored",
    },
  };
}
test(
  "release binary runs help and embedded version without host Node",
  options,
  async (t) => {
    const { root, env } = await fixture(t);
    const help = spawnSync(binary, ["help"], {
      cwd: root,
      env,
      encoding: "utf8",
    });
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /crow setup/);
    const version = spawnSync(binary, ["--version"], {
      cwd: root,
      env,
      encoding: "utf8",
    });
    assert.equal(version.status, 0, version.stderr);
    const pkg = JSON.parse(
      await readFile(new URL("../package.json", import.meta.url), "utf8"),
    );
    assert.ok(version.stdout.includes(pkg.version), version.stdout);
  },
);
test(
  "release binary dispatches its inspection helper without a Node executable",
  options,
  async (t) => {
    const { root, env } = await fixture(t);
    const source = join(root, "source.json");
    await writeFile(
      source,
      JSON.stringify({ dir: root, head: "a".repeat(40), base: "b".repeat(40) }),
    );
    const result = spawnSync(binary, ["_inspection-mcp", source], {
      cwd: root,
      env,
      encoding: "utf8",
      input:
        '{"jsonrpc":"2.0","id":1,"method":"initialize"}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n',
    });
    assert.equal(result.status, 0, result.stderr);
    const messages = result.stdout
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.equal(messages[0].result.serverInfo.name, "crow-inspection");
    assert.deepEqual(messages[1].result.tools.map((tool) => tool.name).sort(), [
      "diff",
      "list_files",
      "read_file",
      "search",
    ]);
  },
);
test(
  "release binary starts its SQLite service and shuts down cleanly",
  options,
  async (t) => {
    const { root, env } = await fixture(t);
    const reservation = createServer();
    reservation.listen(0, "127.0.0.1");
    await once(reservation, "listening");
    const port = reservation.address().port;
    await new Promise((resolve_) => reservation.close(resolve_));
    const config = defaults(root);
    config.role = "service";
    config.port = port;
    config.serviceUrl = `http://127.0.0.1:${port}`;
    await save(config, root);
    const child = spawn(binary, ["run"], {
      cwd: root,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const exited = once(child, "exit");
    let logs = "";
    child.stdout.on("data", (data) => {
      logs += data;
    });
    child.stderr.on("data", (data) => {
      logs += data;
    });
    t.after(async () => {
      if (child.exitCode === null && child.signalCode === null)
        child.kill("SIGTERM");
      await exited;
    });
    const deadline = Date.now() + 10000;
    while (!logs.includes("Crow service running.")) {
      if (
        child.exitCode !== null ||
        child.signalCode !== null ||
        Date.now() > deadline
      )
        assert.fail(`Service failed to start: ${logs}`);
      await new Promise((resolve_) => setTimeout(resolve_, 25));
    }
    const response = await fetch(`${config.serviceUrl}/health`);
    assert.equal(response.status, 200);
    const ready = JSON.parse(await readFile(join(root, "ready.json"), "utf8"));
    assert.equal(ready.pid, child.pid);
    assert.equal(typeof ready.version, "string");
    const db = await readFile(join(root, "service.sqlite"));
    assert.equal(db.subarray(0, 16).toString(), "SQLite format 3\0");
    child.kill("SIGTERM");
    assert.deepEqual(await exited, [0, null], logs);
    await assert.rejects(readFile(join(root, "ready.json")), {
      code: "ENOENT",
    });
  },
);
