import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cleanEnv, hostEnv, processRun } from "../dist/lib/util.mjs";

test("host commands retain user-session locators without exposing them to the provider environment", async () => {
  const values = {
    XDG_RUNTIME_DIR: "/run/user/12345",
    DBUS_SESSION_BUS_ADDRESS: "unix:path=/run/user/12345/bus",
    CROW_TEST_UNRELATED_SECRET: "excluded",
  };
  const previous = Object.fromEntries(Object.keys(values).map((key) => [key, process.env[key]]));
  Object.assign(process.env, values);
  try {
    const host = hostEnv();
    const provider = cleanEnv();
    for (const key of ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]) {
      assert.equal(host[key], values[key]);
      assert.equal(provider[key], undefined);
    }
    assert.equal(host.CROW_TEST_UNRELATED_SECRET, undefined);
    assert.equal(hostEnv({ XDG_RUNTIME_DIR: "/explicit/session" }).XDG_RUNTIME_DIR, "/explicit/session");
    const result = await processRun(process.execPath, ["--input-type=module", "-e",
      "console.log(JSON.stringify({runtime:process.env.XDG_RUNTIME_DIR,bus:process.env.DBUS_SESSION_BUS_ADDRESS,secret:process.env.CROW_TEST_UNRELATED_SECRET}))",
    ]);
    assert.deepEqual(JSON.parse(result.stdout), {
      runtime: values.XDG_RUNTIME_DIR,
      bus: values.DBUS_SESSION_BUS_ADDRESS,
    });
  } finally {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
});

test("interactive inherited commands retain their controlling terminal", {
  skip: process.platform !== "linux" || spawnSync("script", ["--version"]).status !== 0,
}, async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "crow-host-terminal-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const probe = join(directory, "probe.mjs");
  const util = new URL("../dist/lib/util.mjs", import.meta.url).href;
  await writeFile(probe, `import { processRun } from ${JSON.stringify(util)};
await processRun("/bin/sh", ["-c", ": </dev/tty && echo terminal-retained"], { inherit: true });
`);
  const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
  const result = spawnSync("script", ["-q", "-e", "-c", `${quote(process.execPath)} ${quote(probe)}`, "/dev/null"], {
    encoding: "utf8", input: "", timeout: 10000,
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stdout, /terminal-retained/);
});

test("noninteractive commands still start a separate process group and session", {
  skip: process.platform !== "linux",
}, async () => {
  const result = await processRun(process.execPath, ["--input-type=module", "-e", `
import { readFileSync } from "node:fs";
const stat = readFileSync("/proc/self/stat", "utf8");
const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
console.log(JSON.stringify({pid:process.pid,group:Number(fields[2]),session:Number(fields[3])}));
`]);
  const child = JSON.parse(result.stdout);
  assert.equal(child.group, child.pid);
  assert.equal(child.session, child.pid);
});
