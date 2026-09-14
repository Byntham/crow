import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { update } from "../dist/lib/operations.mjs";
import { defaults } from "../dist/lib/config.mjs";

for (const installedCurrent of [false, true]) {
  test(`source update ${installedCurrent ? "leaves a current installation running" : "reinstalls after the checkout was fast-forwarded separately"}`, async (t) => {
    const root = await mkdtemp(join(tmpdir(), "crow-source-update-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const release = join(root, "release"),
      source = join(root, "checkout");
    await mkdir(release);
    const checkout = "b".repeat(40);
    await writeFile(
      join(release, "install.json"),
      JSON.stringify({
        installation: root,
        source,
        bin: join(root, "bin"),
        revision: installedCurrent ? checkout : "a".repeat(40),
      }),
    );
    const actions = [];
    const result = await update(
      root,
      { ...defaults(root), role: "worker" },
      {
        binary: false,
        release,
        run: async (command, args, options) => {
          if (command === "git") {
            assert.equal(options.cwd, source);
            if (args[0] === "status" || args[0] === "fetch")
              return { stdout: "", stderr: "" };
            if (args[0] === "rev-parse")
              return {
                stdout: args[1] === "HEAD" ? checkout : "origin/main\n",
                stderr: "",
              };
            if (args[0] === "rev-list") return { stdout: "0\n", stderr: "" };
            assert.deepEqual(args, ["merge", "--ff-only", "origin/main"]);
            actions.push("merge");
          } else if (command === "systemctl") {
            assert.equal(args[1], "stop");
            actions.push("stop");
          } else if (command === process.execPath) {
            assert.deepEqual(args, ["scripts/check.mjs", "--runtime-only"]);
            actions.push("check");
          } else {
            assert.equal(command, "bash");
            assert.deepEqual(args, ["scripts/install.sh"]);
            assert.equal(options.env.CROW_INSTALL_DIR, root);
            actions.push("install");
          }
          return { stdout: "", stderr: "" };
        },
        installUnit: async () => {
          actions.push("start installed service");
        },
      },
    );
    assert.equal(result.updated, !installedCurrent);
    assert.deepEqual(
      actions,
      installedCurrent
        ? []
        : ["stop", "merge", "check", "install", "start installed service"],
    );
  });
}
