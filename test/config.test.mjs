import test from "node:test";
import assert from "node:assert/strict";
import { defaults, validateConfig, settings } from "../dist/lib/config.mjs";
test("configuration rejects missing credentials and plaintext remote worker transport", () => {
  const c = defaults("/tmp/crow-config-test");
  assert.equal(validateConfig(c), c);
  for (const token of ["", "short", null])
    assert.throws(() => validateConfig({ ...c, adminToken: token }), /tokens/);
  assert.throws(
    () => validateConfig({ ...c, serviceUrl: "http://public.example" }),
    /HTTPS/,
  );
  assert.throws(
    () =>
      validateConfig({ ...c, serviceUrl: "https://user:password@example.com" }),
    /HTTPS/,
  );
  assert.equal(
    validateConfig({ ...c, serviceUrl: "https://crow.example" }).serviceUrl,
    "https://crow.example",
  );
  assert.throws(
    () => validateConfig({ ...c, catchUp: { ...c.catchUp, enabled: "false" } }),
    /boolean/,
  );
});
test("repository overrides reject misspelled settings and preserve worker credentials separately", () => {
  const c = defaults("/tmp/crow-config-test");
  assert.throws(
    () => settings(c, { settings: { reasonning: "high" } }),
    /Unknown/,
  );
  assert.throws(() => settings(c, { settings: { model: 17 } }), /strings/);
  const selected = settings(c, {
    settings: { model: "selected", effort: "high" },
  });
  assert.equal(selected.model, "selected");
  assert.equal(selected.retry.count, 10);
});
test("configuration validates the types required by worker and onboarding contracts", () => {
  const c = defaults("/tmp/crow-config-test");
  for (const worker of [
    { ...c.worker, id: 123 },
    { ...c.worker, codex: false },
    { ...c.worker, codexHome: [] },
    {
      ...c.worker,
      subagents: { mode: "configured", max: 8, model: 42, effort: "high" },
    },
    { ...c.worker, detached: "false" },
  ])
    assert.throws(() => validateConfig({ ...c, worker }));
  assert.throws(() =>
    validateConfig({
      ...c,
      app: { id: 1, pem: false, webhookSecret: "secret", slug: "crow" },
    }),
  );
  assert.throws(() =>
    validateConfig({ ...c, ingress: { type: "funnel", pending: "false" } }),
  );
  assert.throws(() =>
    validateConfig({
      ...c,
      appRegistration: { ownerType: "other", visibility: "public" },
    }),
  );
});
test("sparse repository overrides validate nested values before merging", () => {
  const c = defaults("/tmp/crow-config-test");
  for (const override of [
    { retry: { count: "10" } },
    { subagents: { max: "8" } },
    { subagents: { model: 42 } },
  ])
    assert.throws(() => settings(c, { settings: override }));
  assert.equal(
    settings(c, { settings: { retry: { count: 3 } } }).retry.delayMs,
    5000,
  );
});

test("configuration enum fields reject coercible arrays", () => {
  const c = defaults("/tmp/crow-config-test");
  for (const value of [
    { ...c, role: ["both"] },
    { ...c, ingress: { type: ["funnel"] } },
    {
      ...c,
      worker: {
        ...c.worker,
        subagents: { ...c.worker.subagents, mode: ["inherit"] },
      },
    },
    {
      ...c,
      worker: { ...c.worker, retry: { ...c.worker.retry, mode: ["fixed"] } },
    },
  ])
    assert.throws(() => validateConfig(value));
});
