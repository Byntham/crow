import test from "node:test";
import assert from "node:assert/strict";
import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { defaults } from "../dist/lib/config.mjs";
import {
  unitName,
  unitText,
  update,
  waitForStartup,
} from "../dist/lib/operations.mjs";
import { version } from "../dist/lib/runtime.mjs";

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "crow-binary-operations-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = { ...defaults(root), role: "worker" };
  return { root, config };
}

test("standalone systemd unit starts the stable binary without Node or a script", () => {
  const root = "/tmp/crow home%";
  const executable = join(root, "current", "crow");
  const text = unitText(root, {
    standalone: true,
    executable,
    cli: "/unused/bin/crow.mjs",
    path: "/usr/bin",
  });
  assert.equal(
    text.split("\n").find((line) => line.startsWith("ExecStart=")),
    'ExecStart="/tmp/crow home%%/current/crow" run',
  );
  assert.match(text, /Environment="CROW_HOME=\/tmp\/crow home%%"/);
  assert.match(
    text,
    /Environment="PATH=\/tmp\/crow home%%\/current:\/usr\/bin"/,
  );
  assert(!text.includes("crow.mjs"));
});

test("binary update stages the release before stopping and verifies the restarted worker", async (t) => {
  const { root, config } = await fixture(t);
  await writeFile(join(root, "ready.json"), '{"pid":123,"version":"old"}');
  const events = [];
  const run = async (command, args) => {
    assert.equal(command, "systemctl");
    assert.equal(args[0], "--user");
    assert.equal(args[2], unitName(root));
    events.push(args[1]);
    return { stdout: "" };
  };
  const result = await update(root, config, {
    binary: true,
    run,
    startup: async (requestedRoot, expectedVersion, options) => {
      assert.equal(requestedRoot, root);
      assert.equal(expectedVersion, "0.3.0");
      assert.equal(options.run, run);
      events.push("ready");
    },
    prepareRelease: async (requestedRoot, currentVersion, options) => {
      assert.equal(requestedRoot, root);
      assert.equal(currentVersion, version());
      assert.equal(options.run, run);
      events.push("stage");
      return {
        version: "0.3.0",
        executable: join(root, "releases", "0.3.0", "crow"),
        activate: async () => {
          await assert.rejects(access(join(root, "ready.json")), {
            code: "ENOENT",
          });
          events.push("activate");
          return async () => events.push("rollback");
        },
      };
    },
  });
  assert.deepEqual(result, { updated: true, version: "0.3.0" });
  assert.deepEqual(events, ["stage", "stop", "activate", "start", "ready"]);
});

test("an up-to-date binary leaves the service running", async (t) => {
  const { root, config } = await fixture(t);
  let prepared = false;
  const result = await update(root, config, {
    binary: true,
    startup: async () => assert.fail("No service startup occurred"),
    prepareRelease: async () => {
      prepared = true;
      return null;
    },
    run: async () =>
      assert.fail("A current installation must not stop its service"),
  });
  assert.equal(prepared, true);
  assert.deepEqual(result, { updated: false, message: "Crow is current." });
});

test("release staging failure leaves the service running", async (t) => {
  const { root, config } = await fixture(t);
  await assert.rejects(
    update(root, config, {
      binary: true,
      startup: async () => assert.fail("No service startup occurred"),
      prepareRelease: async () => {
        throw new Error("Release checksum mismatch");
      },
      run: async () =>
        assert.fail("Staging must finish before service changes"),
    }),
    /Release checksum mismatch/,
  );
});

for (const failure of ["start", "readiness"]) {
  test(`binary update rolls back and restarts the old release after ${failure} failure`, async (t) => {
    const { root, config } = await fixture(t);
    const events = [];
    let activeRelease = "old";
    await assert.rejects(
      update(root, config, {
        binary: true,
        startup: async (requestedRoot, expectedVersion) => {
          assert.equal(requestedRoot, root);
          assert.equal(expectedVersion, "0.3.0");
          events.push(`ready:${activeRelease}`);
          if (activeRelease === "new")
            throw new Error("Updated Crow worker did not become ready");
        },
        prepareRelease: async () => ({
          version: "0.3.0",
          executable: join(root, "releases", "0.3.0", "crow"),
          activate: async () => {
            activeRelease = "new";
            events.push("activate");
            return async () => {
              activeRelease = "old";
              events.push("rollback");
            };
          },
        }),
        run: async (command, args) => {
          assert.equal(command, "systemctl");
          assert.equal(args[2], unitName(root));
          events.push(`${args[1]}:${activeRelease}`);
          if (
            args[1] === "start" &&
            activeRelease === "new" &&
            failure === "start"
          )
            throw new Error("New service failed to start");
          return { stdout: "" };
        },
      }),
      failure === "start"
        ? /previous Crow binary was restored: New service failed to start/
        : /previous Crow binary was restored: Updated Crow worker did not become ready/,
    );
    assert.equal(activeRelease, "old");
    assert.deepEqual(events, [
      "stop:old",
      "activate",
      "start:new",
      ...(failure === "readiness" ? ["ready:new"] : []),
      "stop:new",
      "rollback",
      "start:old",
    ]);
  });
}

test("startup readiness requires the expected version and the live systemd process", async (t) => {
  const { root } = await fixture(t);
  const readyPath = join(root, "ready.json");
  const lockPath = join(root, "runtime.lock");
  const ready = { pid: process.pid, version: "0.3.0" };
  const lock = { pid: process.pid };
  const run = async (command, args) => {
    assert.equal(command, "systemctl");
    assert.deepEqual(args, [
      "--user",
      "show",
      unitName(root),
      "--property=MainPID",
      "--value",
    ]);
    return { stdout: `${process.pid}\n` };
  };
  for (const [name, marker, runtime] of [
    ["missing readiness", null, lock],
    ["previous process readiness", { ...ready, pid: process.pid + 1 }, lock],
    ["previous version readiness", { ...ready, version: "0.2.0" }, lock],
    ["missing runtime lock", ready, null],
    ["different runtime process", ready, { pid: process.pid + 1 }],
  ]) {
    await t.test(name, async () => {
      for (const [file, value] of [[readyPath, marker], [lockPath, runtime]]) {
        if (value === null) await rm(file, { force: true });
        else await writeFile(file, JSON.stringify(value));
      }
      await assert.rejects(
        waitForStartup(root, "0.3.0", { run, timeoutMs: 0 }),
        /did not finish starting/,
      );
    });
  }
  await writeFile(readyPath, JSON.stringify(ready));
  await writeFile(lockPath, JSON.stringify(lock));
  await waitForStartup(root, "0.3.0", { run, timeoutMs: 0 });
});
