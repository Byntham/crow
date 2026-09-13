#!/usr/bin/env node
// Development-only protocol fixture: every response is generated on localhost.
// This exercises the installed CLI without spending subscription usage.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, readFile, writeFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync, zstdDecompressSync } from "node:zlib";
import { providerEnvironment, runReview } from "../dist/lib/provider.mjs";

const root = await mkdtemp(join(tmpdir(), "crow-recovery-probe-"));
const binary = process.argv[2] || "codex";
const toml = (value) =>
  Array.isArray(value)
    ? `[${value.map(toml).join(",")}]`
    : value && typeof value === "object"
      ? `{${Object.entries(value)
          .map(([key, val]) => `${JSON.stringify(key)}=${toml(val)}`)
          .join(",")}}`
      : JSON.stringify(value);
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function until(check, message, ms = 20000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const value = await check();
    if (value) return value;
    await pause(25);
  }
  throw new Error(message);
}
const report = {
  summary:
    "Protocol fixture completed; no inference or code review was performed.",
  findings: [],
};
let server,
  initial,
  initialFailure,
  phase = "interrupt",
  mainCalls = 0,
  childCalls = 0,
  fixtureError;
const captures = [],
  held = [],
  sessions = [],
  controller = new AbortController();
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
function complete(response) {
  reply(response, {
    type: "message",
    id: "msg_fixture",
    role: "assistant",
    status: "completed",
    content: [
      { type: "output_text", text: JSON.stringify(report), annotations: [] },
    ],
  });
}
function tool(response, input, suffix) {
  reply(response, {
    type: "custom_tool_call",
    id: `ct_${suffix}`,
    call_id: `call_${suffix}`,
    name: "exec",
    namespace: "functions",
    input,
  });
}
const taskDir = join(root, "reviews", "probe", "tasks");
async function savedTask() {
  for (const name of await readdir(taskDir).catch(() => [])) {
    if (/^[a-f0-9]+\.json$/.test(name))
      return JSON.parse(await readFile(join(taskDir, name), "utf8"));
  }
  return null;
}
try {
  const worker = {
    codex: binary,
    codexHome: join(root, "codex"),
    model: "gpt-6-astra",
    effort: "medium",
    subagents: { mode: "inherit", max: 2 },
    retry: { mode: "fixed", count: 0, delayMs: 5000 },
    timeoutMs: 30000,
  };
  await providerEnvironment(worker, root);
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
    .map((value) => Buffer.from(JSON.stringify(value)).toString("base64url"))
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
  server = createServer(async (request, response) => {
    try {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      let bytes = Buffer.concat(chunks);
      if (request.headers["content-encoding"] === "gzip")
        bytes = gunzipSync(bytes);
      if (request.headers["content-encoding"] === "zstd")
        bytes = zstdDecompressSync(bytes);
      if (request.url !== "/responses") {
        response.writeHead(400);
        response.end(
          JSON.stringify({ error: { message: "Local metadata fixture" } }),
        );
        return;
      }
      const body = JSON.parse(bytes),
        child = JSON.stringify(body.input).includes(
          "Your bounded delegated task:",
        );
      captures.push({ phase, child, body });
      if (child) childCalls++;
      else mainCalls++;
      if (phase === "interrupt") {
        if (!child && mainCalls === 1) {
          tool(
            response,
            'const tool=ALL_TOOLS.find(t=>t.name.endsWith("__start_review_task")); text(await tools[tool.name]({task:"Resume this bounded localhost fixture after interruption."}));',
            "start",
          );
          return;
        }
        const stream = { child, closed: false };
        held.push(stream);
        response.on("close", () => {
          stream.closed = true;
        });
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.write(
          'event: response.created\ndata: {"type":"response.created","response":{"id":"held","status":"in_progress"}}\n\n',
        );
        return;
      }
      if (child) {
        complete(response);
        return;
      }
      if (mainCalls === 3) {
        const task = await savedTask();
        tool(
          response,
          `const tool=ALL_TOOLS.find(t=>t.name.endsWith("resume_review_task")); text(await tools[tool.name]({id:${JSON.stringify(task.id)}}));`,
          "resume",
        );
        return;
      }
      await until(
        async () => (await savedTask())?.state === "completed",
        "Resumed child did not complete",
      );
      complete(response);
    } catch (error) {
      fixtureError = error;
      if (!response.headersSent) response.writeHead(500);
      response.end(JSON.stringify({ error: { message: error.message } }));
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}`;
  const transport = {
    chatgpt_base_url: url,
    "model_providers.crow-probe.name": "Crow localhost fixture",
    "model_providers.crow-probe.requires_openai_auth": true,
    "model_providers.crow-probe.wire_api": "responses",
    "model_providers.crow-probe.base_url": url,
    "model_providers.crow-probe.supports_websockets": false,
    "model_providers.crow-probe.request_max_retries": 0,
    "model_providers.crow-probe.stream_max_retries": 0,
  };
  const transportArgs = Object.entries(transport).flatMap(([key, value]) => [
    "-c",
    `${key}=${toml(value)}`,
  ]);
  const wrapper = join(root, "codex-local-fixture");
  // Log fixture-owned process identities to verify that cancellation leaves no live children.
  const processLog = join(root, "processes.jsonl");
  await writeFile(
    wrapper,
    `#!${process.execPath}\nimport {spawn} from 'node:child_process';\nimport {appendFileSync} from 'node:fs';\nconst args=process.argv.slice(2).map(x=>x==='model_provider="openai"'?'model_provider="crow-probe"':x);\nconst child=spawn(${JSON.stringify(binary)},[...${JSON.stringify(transportArgs)},...args],{stdio:'inherit'});\nappendFileSync(${JSON.stringify(processLog)},JSON.stringify({wrapper:process.pid,pid:child.pid,exec:args.includes('exec')})+'\\n');\nfor(const sig of ['SIGINT','SIGTERM'])process.on(sig,()=>child.kill(sig));\nchild.on('exit',(code)=>process.exit(code??1));\n`,
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
  const context = {
    root,
    job,
    source: { dir: root, ...job.comparison },
    guidance: { files: [] },
    onSession: (session) => {
      sessions.push(session);
    },
  };
  initial = runReview({ ...context, signal: controller.signal }).then(
    () => {
      initialFailure = new Error("Interrupted review unexpectedly completed");
      return initialFailure;
    },
    (error) => {
      initialFailure = error;
      return error;
    },
  );
  await until(async () => {
    if (initialFailure) throw initialFailure;
    if (fixtureError) throw fixtureError;
    return (
      held.some((stream) => stream.child) &&
      held.some((stream) => !stream.child) &&
      (await savedTask())?.session &&
      sessions.length
    );
  }, "Parent and child were not both running with saved sessions");
  const before = await savedTask(),
    parentSession = sessions[0];
  controller.abort(new Error("Fixture requested interruption"));
  const interruption = await initial;
  assert.match(interruption.message, /Fixture requested interruption/);
  await until(
    () => held.every((stream) => stream.closed),
    "An interrupted inference stream remained open",
  );
  const paused = await until(async () => {
    const task = await savedTask();
    return task?.state === "paused" && task;
  }, "Helper did not persist a paused child");
  assert.equal(paused.session, before.session);
  const processes = (await readFile(processLog, "utf8"))
    .trim()
    .split("\n")
    .map(JSON.parse)
    .filter((entry) => entry.exec);
  assert.equal(
    processes.length,
    2,
    "Expected main and delegated exec invocations",
  );
  for (const entry of processes)
    for (const pid of [entry.wrapper, entry.pid]) {
      await until(async () => {
        try {
          const stat = await readFile(`/proc/${pid}/stat`, "utf8");
          return /\) Z /.test(stat);
        } catch (error) {
          if (error.code === "ENOENT") return true;
          throw error;
        }
      }, `Interrupted process ${pid} remained live`);
    }
  console.log(
    "PASS: interruption closes parent and delegated response streams and stops both exec processes; child state is paused.",
  );
  phase = "resume";
  const completed = await runReview({
    ...context,
    job: { ...job, session: parentSession, resumeEpoch: 1 },
  });
  if (fixtureError) throw fixtureError;
  assert.deepEqual(completed.findings, []);
  assert.equal(completed.summary, report.summary);
  assert.equal(sessions.at(-1), parentSession);
  const resumedTask = await savedTask();
  assert.equal(resumedTask.state, "completed");
  assert.equal(resumedTask.session, before.session);
  assert.equal(childCalls, 2);
  const childEvents = (
    await readFile(join(taskDir, before.id, "runtime", "events.jsonl"), "utf8")
  )
    .trim()
    .split("\n")
    .map(JSON.parse)
    .filter((event) => event.type === "thread.started");
  assert.equal(childEvents.length, 2);
  assert.ok(childEvents.every((event) => event.thread_id === before.session));
  for (const capture of captures)
    assert.equal(
      capture.body.text?.format?.type,
      "json_schema",
      "Each initial/resumed inference request must receive the output schema",
    );
  console.log(
    "PASS: explicit parent and child resumes retain their original session IDs and accept completed schema-constrained reports.",
  );
  console.log(
    "NOT VERIFIED: real provider interruption behavior, lossless in-flight reasoning recovery, or concurrent subscription token refresh.",
  );
} catch (error) {
  throw fixtureError || error;
} finally {
  controller.abort(new Error("Fixture cleanup"));
  await initial;
  server?.closeAllConnections();
  server?.close();
  await rm(root, { recursive: true, force: true });
}
