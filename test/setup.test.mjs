import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  chooseFunnelPort,
  configureFunnel,
  appManifest,
  registerApp,
  registrationAction,
  applySetupPort,
} from "../dist/lib/setup.mjs";
import { defaults } from "../dist/lib/config.mjs";
import {
  unitText,
  admin,
  updateAvailability,
} from "../dist/lib/operations.mjs";
import { parse, redact, policyChanges } from "../dist/bin/crow.mjs";

test("Funnel uses a free port and only reuses the exact Crow route", () => {
  const status = {
    TCP: { 443: { HTTPS: true }, 8443: { HTTPS: true } },
    Web: {
      "host:443": { Handlers: { "/": { Proxy: "http://localhost:3773" } } },
      "host:8443": { Handlers: { "/": { Proxy: "http://127.0.0.1:8787" } } },
    },
  };
  assert.equal(chooseFunnelPort(status), 10000);
  assert.equal(chooseFunnelPort(status, 8443, "http://127.0.0.1:8787"), 8443);
  status.Web["host:8443"].Handlers["/private"] = {
    Proxy: "http://localhost:1234",
  };
  assert.equal(chooseFunnelPort(status, 8443, "http://127.0.0.1:8787"), 10000);
  status.TCP[10000] = { HTTPS: true };
  assert.throws(() => chooseFunnelPort(status), /in use/);
});
test("Funnel setup preserves existing Serve routes and uses background persistence", async () => {
  const calls = [],
    c = defaults("/tmp/crow");
  const run = async (command, args) => {
    calls.push([command, args]);
    if (args[0] === "status")
      return {
        stdout: JSON.stringify({
          BackendState: "Running",
          Self: { DNSName: "crow.example.ts.net." },
        }),
      };
    if (args[0] === "serve")
      return {
        stdout: JSON.stringify({ TCP: { 443: { HTTPS: true } }, Web: {} }),
      };
    return { stdout: "" };
  };
  await configureFunnel(c, { run });
  assert.equal(c.publicUrl, "https://crow.example.ts.net:8443");
  assert.deepEqual(calls.at(-1), [
    "tailscale",
    ["funnel", "--bg", "--https=8443", "http://127.0.0.1:8787"],
  ]);
  assert(!calls.flat(2).includes("reset"));
});
test("systemd unit quotes paths, persists across logout, and kills child processes", () => {
  const text = unitText("/tmp/crow home%", {
    executable: "/node space",
    cli: "/app/bin/crow.mjs",
    path: "/usr/bin",
  });
  assert.match(text, /ExecStart="\/node space"/);
  assert.match(text, /CROW_HOME=\/tmp\/crow home%%/);
  assert.match(text, /KillMode=mixed/);
  assert.match(text, /UMask=0077/);
});
test("CLI preserves JSON and redacts credentials", () => {
  assert.deepEqual(
    parse(["repo-config", "a/b", "--json", '{"model":"m"}']).flags,
    { json: '{"model":"m"}' },
  );
  const c = defaults("/tmp");
  c.app = { pem: "secret", webhookSecret: "secret", id: 1 };
  const text = JSON.stringify(redact(c));
  assert(!text.includes(c.worker.token));
  assert(!text.includes("secret"));
});
test("worker-only CLI cannot use connection-service administration", async () => {
  await assert.rejects(
    admin({ ...defaults("/tmp"), role: "worker" }, "status", undefined, {
      fetcher: () => {
        throw new Error("must not call");
      },
    }),
    /connection-service host/,
  );
});
test("manifest requests advisory review permissions and points to configured public callback", () => {
  const c = defaults("/tmp");
  c.publicUrl = "https://crow.example:8443";
  const m = appManifest(c, "Crow");
  assert.equal(m.public, true);
  assert.equal(m.default_permissions.contents, "read");
  assert.equal(m.default_permissions.checks, undefined);
  assert.equal(m.redirect_url, c.publicUrl + "/setup/callback");
});
test("App registration rejects forged callback and stores generated credentials after valid state", async () => {
  const root = await mkdtemp(join(tmpdir(), "crow-setup-"));
  const c = defaults(root);
  c.publicUrl = "https://example.com";
  c.port = 0;
  c.operator = "alice";
  // Obtain a free concrete port. Registration binds locally, the fixture uses the same route a proxy forwards.
  const { createServer } = await import("node:net");
  const probe = createServer();
  await new Promise((r) => probe.listen(0, "127.0.0.1", r));
  c.port = probe.address().port;
  await new Promise((r) => probe.close(r));
  let url;
  const announced = new Promise((resolve) => (url = resolve));
  let exchanges = 0;
  const registration = registerApp(c, root, {
    ask: async (label, fallback) =>
      label === "GitHub App name" ? "<Crow>" : fallback,
    timeoutMs: 5000,
    log: (line) => {
      if (line.startsWith("Open this URL")) url(line.split(" ").at(-1));
    },
    fetcher: async () => {
      exchanges++;
      return {
        ok: true,
        json: async () => ({
          id: 42,
          pem: "key",
          webhook_secret: "secret",
          slug: "crow-test",
        }),
      };
    },
  });
  try {
    const announcedUrl = new URL(await announced),
      base = `http://127.0.0.1:${c.port}`;
    assert.equal(
      (await fetch(`${base}/setup/callback?state=bad&code=bad`)).status,
      400,
    );
    assert.equal(exchanges, 0);
    const form = await (await fetch(base + announcedUrl.pathname)).text();
    const state = form.match(/apps\/new\?state=([a-f0-9]+)/)[1];
    assert(form.includes("&lt;Crow&gt;"));
    const response = await fetch(
      `${base}/setup/callback?state=${state}&code=valid`,
    );
    assert.equal(response.status, 200);
    await registration;
    const saved = JSON.parse(await readFile(join(root, "config.json"), "utf8"));
    assert.equal(saved.app.id, 42);
    assert.equal(saved.app.pem, "key");
    assert.equal(exchanges, 1);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("organization App registration URL is fixed to GitHub and rejects injected path/markup", () => {
  const state = "a".repeat(64);
  assert.equal(
    registrationAction(
      { ownerType: "organization", organization: "my-team" },
      state,
    ),
    `https://github.com/organizations/my-team/settings/apps/new?state=${state}`,
  );
  for (const organization of [
    "../settings",
    "x/../../evil",
    'x\" onclick=\"evil',
    "https://evil.test",
    "-bad",
  ])
    assert.throws(
      () =>
        registrationAction({ ownerType: "organization", organization }, state),
      /Invalid/,
    );
  assert.throws(
    () => registrationAction({ ownerType: "other" }, state),
    /Invalid/,
  );
  const c = defaults("/tmp");
  c.publicUrl = "https://crow.example";
  c.appRegistration = { visibility: "private" };
  assert.equal(appManifest(c, "Crow").public, false);
});

test("Funnel persists its plan before mutation and resumes the same binding after interruption", async () => {
  const config = defaults("/tmp/crow");
  let saved,
    fail = true;
  const status = { TCP: { 443: { HTTPS: true } }, Web: {} };
  const ports = [];
  const run = async (command, args) => {
    if (args[0] === "status")
      return {
        stdout: JSON.stringify({
          BackendState: "Running",
          Self: { DNSName: "crow.example.ts.net." },
        }),
      };
    if (args[0] === "serve") return { stdout: JSON.stringify(status) };
    assert.equal(saved.ingress.pending, true);
    assert.equal(saved.publicUrl, "https://crow.example.ts.net:8443");
    ports.push(args[2]);
    status.TCP[8443] = { HTTPS: true };
    status.Web["crow.example.ts.net:8443"] = {
      Handlers: { "/": { Proxy: "http://127.0.0.1:8787" } },
    };
    if (fail) throw new Error("Interrupted after binding");
    return { stdout: "" };
  };
  const persistPlan = (value) => {
    saved = structuredClone(value);
  };
  await assert.rejects(
    configureFunnel(config, { run, persistPlan }),
    /Interrupted/,
  );
  assert.equal(saved.ingress.pending, true);
  fail = false;
  await configureFunnel(saved, { run, persistPlan });
  assert.deepEqual(ports, ["--https=8443", "--https=8443"]);
  assert.equal(saved.ingress.pending, undefined);
});

test("setup port is configurable before onboarding and refuses accidental route migrations", () => {
  const config = defaults("/tmp/crow");
  applySetupPort(config, "9887");
  assert.equal(config.port, 9887);
  assert.equal(config.serviceUrl, "http://127.0.0.1:9887");
  config.publicUrl = "https://crow.example";
  assert.throws(() => applySetupPort(config, 9888), /migration/);
  assert.throws(() => applySetupPort(config, true), /setup port/);
  assert.throws(() => applySetupPort(config, "garbage"), /setup port/);
});

test("requester policy changes leave PR author policy untouched", () => {
  assert.deepEqual(policyChanges({ requesters: "Alice,bob" }), {
    requesters: ["alice", "bob"],
  });
  assert.deepEqual(policyChanges({ everyone: true }), { policy: "everyone" });
  assert.deepEqual(policyChanges({ authors: "alice", requesters: "bob" }), {
    policy: "selected",
    authors: ["alice"],
    requesters: ["bob"],
  });
  assert.throws(
    () => policyChanges({ authors: "alice,", everyone: true }),
    /Choose/,
  );
  for (const requesters of [true, "", "../alice", "alice,,bob", "-alice"])
    assert.throws(() => policyChanges({ requesters }), /GitHub/);
});

test("status update check caches daily without merging and tolerates network failure", async () => {
  const root = await mkdtemp(join(tmpdir(), "crow-updates-"));
  const calls = [];
  const run = async (command, args, options) => {
    calls.push({ command, args, options });
    if (args[0] === "fetch") return { stdout: "" };
    if (args[0] === "rev-parse") return { stdout: "origin/main\n" };
    if (args[0] === "rev-list") return { stdout: "2\n" };
    throw new Error("Unexpected git mutation");
  };
  try {
    assert.equal(
      (await updateAvailability(root, { run, now: 100000 })).available,
      true,
    );
    assert.equal(calls[0].options.timeout, 10000);
    assert.equal(
      calls[0].options.cwd,
      fileURLToPath(new URL("..", import.meta.url)).replace(/\/$/, ""),
    );
    assert.equal(calls.length, 3);
    assert.equal(
      (await updateAvailability(root, { run, now: 100001 })).cached,
      true,
    );
    assert.equal(calls.length, 3);
    const failed = await updateAvailability(root, {
      run: async () => {
        throw new Error("offline");
      },
      now: 100000 + 86400001,
    });
    assert.equal(failed.available, true);
    assert.match(failed.warning, /offline/);
    assert.equal(
      (await updateAvailability(root, { run, now: 100000 + 86400002 })).cached,
      true,
    );
    assert.equal(calls.length, 3);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("Funnel can guide a privileged retry without changing other bindings", async () => {
  const c = defaults("/tmp/crow"),
    calls = [];
  const run = async (command, args) => {
    calls.push([command, args]);
    if (args[0] === "status")
      return {
        stdout: JSON.stringify({
          BackendState: "Running",
          Self: { DNSName: "host.ts.net" },
        }),
      };
    if (args[0] === "serve")
      return {
        stdout: JSON.stringify({ TCP: { 443: { HTTPS: true } }, Web: {} }),
      };
    if (command === "tailscale" && args[0] === "funnel")
      throw new Error("Access denied");
    return { stdout: "" };
  };
  await configureFunnel(c, { run, ask: async () => "yes" });
  assert.deepEqual(calls.at(-1), [
    "sudo",
    ["tailscale", "funnel", "--bg", "--https=8443", "http://127.0.0.1:8787"],
  ]);
  assert.equal(c.ingress.pending, undefined);
});

test("update checks accept release metadata without a Git revision", async () => {
  const release = await mkdtemp(join(tmpdir(), "crow-release-"));
  const calls = [];
  const source = join(release, "source");
  const run = async (command, args, options) => {
    calls.push({ command, args, options });
    if (args[0] === "fetch") return { stdout: "" };
    if (args[0] === "rev-parse") return { stdout: "origin/main\n" };
    if (args[0] === "rev-list") return { stdout: "0\n" };
    throw new Error("Unexpected command");
  };
  try {
    await writeFile(
      join(release, "install.json"),
      JSON.stringify({ source, revision: null }),
    );
    const result = await updateAvailability(join(release, "state"), {
      release,
      run,
      now: 100000,
    });
    assert.equal(result.available, false);
    assert.equal(result.warning, undefined);
    assert.equal(calls.length, 3);
    assert(calls.every(({ options }) => options.cwd === source));
    assert.deepEqual(calls[2].args, [
      "rev-list",
      "--count",
      "HEAD..origin/main",
    ]);
  } finally {
    await rm(release, { recursive: true, force: true });
  }
});
