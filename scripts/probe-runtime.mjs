#!/usr/bin/env node
// Development protocol probe. It sends no requests to an inference provider.
// All model responses below come from this process's localhost fixture.
import { createServer } from "node:http";
import { mkdtemp, mkdir, rm, writeFile, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { gunzipSync, zstdDecompressSync } from "node:zlib";
import {
  diagnostics,
  prepareReview,
  providerEnvironment,
} from "../dist/lib/provider.mjs";
import { processRun } from "../dist/lib/util.mjs";
const root = await mkdtemp(join(tmpdir(), "crow-runtime-probe-"));
const binary = process.argv[2] || "codex";
const toml = (v) =>
  Array.isArray(v)
    ? `[${v.map(toml).join(",")}]`
    : v && typeof v === "object"
      ? `{${Object.entries(v)
          .map(([k, v]) => `${JSON.stringify(k)}=${toml(v)}`)
          .join(",")}}`
      : JSON.stringify(v);
const captures = [];
let server;
function reply(response, item) {
  response.writeHead(200, { "content-type": "text/event-stream" });
  for (const event of [
    {
      type: "response.created",
      response: { id: "resp_fixture", status: "in_progress" },
    },
    { type: "response.output_item.added", output_index: 0, item },
    { type: "response.output_item.done", output_index: 0, item },
    {
      type: "response.completed",
      response: {
        id: "resp_fixture",
        status: "completed",
        output: [item],
        usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
      },
    },
  ])
    response.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
  response.end();
}
function completed(response) {
  reply(response, {
    type: "message",
    id: "msg_fixture",
    role: "assistant",
    status: "completed",
    content: [
      {
        type: "output_text",
        text: JSON.stringify({
          summary:
            "Protocol fixture completed; no code review or inference was performed.",
          findings: [],
        }),
        annotations: [],
      },
    ],
  });
}
try {
  const worker = {
    codex: binary,
    codexHome: join(root, "codex"),
    model: "gpt-6-astra",
    effort: "medium",
    subagents: {
      mode: "configured",
      max: 2,
      model: process.argv[3] || "gpt-5.6-sol",
      effort: "low",
    },
  };
  const checks = await diagnostics(worker, root);
  console.log(JSON.stringify(checks, null, 2));
  if (!checks.ok) throw new Error("Required runtime controls are unavailable");
  // Demonstrate the dedicated-state guard because max project-doc bytes does not suppress global AGENTS.md.
  await writeFile(
    join(worker.codexHome, "AGENTS.md"),
    "CROW_UNRELATED_INSTRUCTION_SENTINEL",
  );
  let rejected = false;
  try {
    await providerEnvironment(worker, root);
  } catch (e) {
    rejected = e.kind === "config";
  }
  if (!rejected) throw new Error("Global instruction guard failed");
  await rm(join(worker.codexHome, "AGENTS.md"));
  const jwt = [
    { alg: "none" },
    {
      sub: "fixture",
      aud: "fixture",
      iss: "fixture",
      exp: 1999999999,
      email: "fixture@example.invalid",
      "https://api.openai.com/auth": {
        chatgpt_user_id: "fixture",
        chatgpt_account_id: "fixture",
        chatgpt_plan_type: "pro",
      },
    },
    { fixture: true },
  ]
    .map((x) => Buffer.from(JSON.stringify(x)).toString("base64url"))
    .join(".");
  await writeFile(
    join(worker.codexHome, "auth.json"),
    JSON.stringify({
      auth_mode: "chatgpt",
      OPENAI_API_KEY: null,
      tokens: {
        id_token: jwt,
        access_token: jwt,
        refresh_token: "fixture-only",
        account_id: "fixture",
      },
      last_refresh: new Date().toISOString(),
    }),
    { mode: 0o600 },
  );
  let primaryCalls = 0,
    childCalls = 0;
  server = createServer(async (req, res) => {
    try {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      let body = Buffer.concat(chunks);
      if (req.headers["content-encoding"] === "gzip") body = gunzipSync(body);
      if (req.headers["content-encoding"] === "zstd")
        body = zstdDecompressSync(body);
      if (req.url !== "/responses") {
        res.writeHead(400, { "content-type": "application/json" });
        res.end(
          JSON.stringify({ error: { message: "Local metadata fixture" } }),
        );
        return;
      }
      const request = JSON.parse(body);
      captures.push(request);
      const input = JSON.stringify(request.input),
        child = input.includes("Your bounded delegated task:");
      if (child) {
        childCalls++;
        if (childCalls === 1) {
          reply(res, {
            type: "custom_tool_call",
            id: "ct_child",
            call_id: "call_child",
            name: "exec",
            namespace: "functions",
            input:
              "text({fixtureTools:ALL_TOOLS.map(t=>t.name),fixtureGlobals:{process:typeof process,require:typeof require,fetch:typeof fetch,WebSocket:typeof WebSocket,Deno:typeof Deno,Bun:typeof Bun}});",
          });
          return;
        }
        completed(res);
        return;
      }
      primaryCalls++;
      if (primaryCalls === 1) {
        reply(res, {
          type: "custom_tool_call",
          id: "ct_fixture",
          call_id: "call_fixture",
          name: "exec",
          namespace: "functions",
          input:
            'text({fixtureTools:ALL_TOOLS.map(t=>t.name),fixtureGlobals:{process:typeof process,require:typeof require,fetch:typeof fetch,WebSocket:typeof WebSocket,Deno:typeof Deno,Bun:typeof Bun}}); const tool=ALL_TOOLS.find(t=>t.name.endsWith("__start_review_task")); text(await tools[tool.name]({task:"Verify the bounded fixture; no inference is performed."}));',
        });
        return;
      }
      // The helper saves the completed child before publishing its result.
      for (let i = 0; i < 100 && childCalls < 2; i++)
        await new Promise((r) => setTimeout(r, 50));
      if (
        !captures.some((r) =>
          JSON.stringify(r.input).includes("Your bounded delegated task:"),
        )
      )
        throw new Error("Delegated fixture did not reach the local endpoint");
      completed(res);
    } catch (e) {
      res.writeHead(400, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: e.message } }));
    }
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const url = `http://127.0.0.1:${server.address().port}`;
  const transport = {
    chatgpt_base_url: url,
    "model_providers.crow-probe.name": "Crow localhost protocol fixture",
    "model_providers.crow-probe.requires_openai_auth": true,
    "model_providers.crow-probe.wire_api": "responses",
    "model_providers.crow-probe.base_url": url,
    "model_providers.crow-probe.supports_websockets": false,
    "model_providers.crow-probe.request_max_retries": 0,
    "model_providers.crow-probe.stream_max_retries": 0,
  };
  const transportArgs = Object.entries(transport).flatMap(([k, v]) => [
    "-c",
    `${k}=${toml(v)}`,
  ]);
  // Test-only wrapper redirects every Codex process, including metadata and children, to localhost.
  // Production provider code has no alternate-endpoint switch or API-key fallback.
  const wrapper = join(root, "codex-local-fixture");
  await writeFile(
    wrapper,
    `#!${process.execPath}\nimport {spawn} from 'node:child_process';\nconst args=process.argv.slice(2).map(x=>x==='model_provider="openai"'?'model_provider="crow-probe"':x);\nconst child=spawn(${JSON.stringify(binary)},[...${JSON.stringify(transportArgs)},...args],{stdio:'inherit'});\nfor(const sig of ['SIGINT','SIGTERM'])process.on(sig,()=>child.kill(sig));\nchild.on('exit',(code)=>process.exit(code??1));\n`,
    { mode: 0o700 },
  );
  worker.codex = wrapper;
  const job = {
    id: "probe",
    repo: "fixture/repository",
    number: 1,
    settings: worker,
    comparison: { head: "a".repeat(40), base: "b".repeat(40), target: "main" },
  };
  const source = { dir: root, ...job.comparison };
  let p = await prepareReview({ root, job, source, guidance: { files: [] } });
  let args = Object.entries(p.config).flatMap(([k, v]) => [
    "-c",
    `${k}=${toml(v)}`,
  ]);
  await processRun(
    wrapper,
    [...args, "debug", "prompt-input", "Compatibility probe only."],
    { env: p.env, cwd: p.cwd, timeout: 30000 },
  );
  p = await prepareReview({ root, job, source, guidance: { files: [] } });
  args = Object.entries(p.config).flatMap(([k, v]) => [
    "-c",
    `${k}=${toml(v)}`,
  ]);
  const prompt = await processRun(
    wrapper,
    [...args, "debug", "prompt-input", "Compatibility probe only."],
    { env: p.env, cwd: p.cwd, timeout: 30000 },
  );
  if (
    prompt.stdout.includes("CROW_UNRELATED_INSTRUCTION_SENTINEL") ||
    prompt.stdout.includes("Available skills")
  )
    throw new Error("Unrelated instructions or skills leaked into the prompt");
  const result = await processRun(
    wrapper,
    [
      ...args,
      "exec",
      "--ignore-user-config",
      "--ignore-rules",
      "--strict-config",
      "--skip-git-repo-check",
      "--json",
      "-",
    ],
    {
      env: p.env,
      cwd: p.cwd,
      input: "Local protocol fixture only. No real review.",
      timeout: 30000,
    },
  );
  const child = captures.find((r) =>
    JSON.stringify(r.input).includes("Your bounded delegated task:"),
  );
  if (!child) throw new Error("No child request captured");
  if (
    child.model !== worker.subagents.model ||
    child.reasoning?.effort !== worker.subagents.effort
  )
    throw new Error("Delegated model or reasoning was not enforced");
  const inventory = [];
  for (const r of captures) {
    for (const entry of r.input) {
      if (entry.type !== "custom_tool_call_output") continue;
      const text =
        typeof entry.output === "string"
          ? entry.output
          : JSON.stringify(entry.output);
      inventory.push({
        child: JSON.stringify(r.input).includes("Your bounded delegated task:"),
        text,
      });
    }
  }
  const allowed = new Set([
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
    "clock__curr_time",
    "mcp__crow_inspection__list_files",
    "mcp__crow_inspection__read_file",
    "mcp__crow_inspection__search",
    "mcp__crow_inspection__diff",
    "mcp__crow_inspection__start_review_task",
    "mcp__crow_inspection__review_task_status",
    "mcp__crow_inspection__resume_review_task",
    "mcp__crow_inspection__restart_review_task",
    "mcp__crow_inspection__wait_review_task",
  ]);
  if (!inventory.some((x) => x.child) || !inventory.some((x) => !x.child))
    throw new Error("Missing main or child Code Mode inventory");
  for (const item of inventory) {
    // The model-visible output contains a JSON text content block emitted by text().
    const output = JSON.parse(item.text);
    const blocks = Array.isArray(output) ? output : output.content || [];
    const found = blocks
      .map((b) => {
        try {
          return JSON.parse(b.text);
        } catch {
          return null;
        }
      })
      .find((b) => b?.fixtureTools);
    if (!found)
      throw new Error(
        "Code Mode did not return its actual nested tool inventory",
      );
    if (
      Object.values(found.fixtureGlobals || {}).some((v) => v !== "undefined")
    )
      throw new Error("Code Mode unexpectedly exposes a host/network global");
    for (const name of found.fixtureTools)
      if (!allowed.has(name))
        throw new Error(`Unexpected nested tool: ${name}`);
    if (
      item.child &&
      found.fixtureTools.some((n) => n.endsWith("start_review_task"))
    )
      throw new Error("Child can bypass delegation ceiling");
  }
  console.log(
    "PASS: actual Codex requests expose inspection tools only; native delegation and execution tools are absent.",
  );
  console.log(
    "PASS: Crow delegation preserves configured child model/reasoning and disables nested delegation.",
  );
  console.log(
    "PASS: unrelated global instructions are rejected and bundled skills are absent from the controlled prompt.",
  );
  console.log(
    "NOT VERIFIED: live subscription token refresh races, provider-side behavior during real inference, or lossless interruption recovery.",
  );
} finally {
  server?.closeAllConnections();
  server?.close();
  await rm(root, { recursive: true, force: true });
}
