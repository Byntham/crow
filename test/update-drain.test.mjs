import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { defaults } from "../dist/lib/config.mjs";
import { update } from "../dist/lib/operations.mjs";
import { Store } from "../dist/lib/store.mjs";

for (const binary of [false, true]) {
  for (const scenario of [
    "success",
    "operator drain",
    "lost drain response",
    "status failure",
    "stop failure",
    "start failure",
    "readiness failure",
  ]) {
    test(`${binary ? "binary" : "source"} update restores only its own drain after ${scenario}`, async (t) => {
      const root = await mkdtemp(join(tmpdir(), "crow-update-drain-"));
      const store = new Store(join(root, "service.sqlite"));
      t.after(async () => {
        store.close();
        await rm(root, { recursive: true, force: true });
      });
      const operatorDrain = scenario === "operator drain";
      if (operatorDrain) store.put("state", "drain", true);
      let online = true,
        statusCalls = 0,
        drainCalls = 0,
        undrainCalls = 0,
        readinessCalls = 0;
      const updating = update(
        root,
        { ...defaults(root), role: "service" },
        {
          binary,
          release: root,
          prepareRelease: async () => ({
            version: "0.3.0",
            executable: join(root, "crow"),
            activate: async () => async () => {},
          }),
          startup: async () => {},
          ready: async () => {
            readinessCalls++;
            if (scenario === "readiness failure") throw new Error(scenario);
            online = true;
          },
          administer: async (_config, action) => {
            if (action === "undrain") undrainCalls++;
            if (!online) throw new Error("Listener unavailable");
            if (action === "status") {
              statusCalls++;
              if (statusCalls > 1 && scenario === "status failure")
                throw new Error(scenario);
              return {
                jobs: [],
                repos: [],
                draining: !!store.get("state", "drain"),
              };
            }
            if (action === "drain") {
              drainCalls++;
              store.put("state", "drain", true);
              if (scenario === "lost drain response") {
                online = false;
                throw new Error(scenario);
              }
            } else if (action === "undrain") store.delete("state", "drain");
            return {};
          },
          run: async (command, args) => {
            if (command === "git") {
              if (args[0] === "rev-parse") return { stdout: "origin/main\n" };
              if (args[0] === "rev-list") return { stdout: "1\n" };
              return { stdout: "" };
            }
            if (command === "systemctl") {
              online = false;
              if (args[1] === "stop" && scenario === "stop failure")
                throw new Error(scenario);
              if (args[1] === "start" && scenario === "start failure")
                throw new Error(scenario);
            }
            return { stdout: "" };
          },
        },
      );
      if (scenario === "success" || operatorDrain) {
        assert.equal((await updating).updated, true);
        assert.equal(readinessCalls, 1);
      } else await assert.rejects(updating, new RegExp(scenario));
      assert.equal(!!store.get("state", "drain"), operatorDrain);
      assert.equal(drainCalls, operatorDrain ? 0 : 1);
      assert.equal(undrainCalls, operatorDrain ? 0 : 1);
    });
  }
}
