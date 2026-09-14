import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readFile, rm, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
  discover,
  authStatus,
  diagnostics,
  runReview,
  classifyError,
  providerEnvironment,
  prepareReview,
} from "../dist/lib/provider.mjs";

async function fixture(t, options = {}) {
  const root = await mkdtemp(join(tmpdir(), "crow-provider-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const codex = join(root, "fake-codex");
  await writeFile(join(root, "behavior.json"), JSON.stringify(options));
  await writeFile(
    codex,
    `#!/usr/bin/env node
 const fs=require('node:fs'),readline=require('node:readline');
 const root=${JSON.stringify(root)},args=process.argv.slice(2),behavior=JSON.parse(fs.readFileSync(root+'/behavior.json'));
 if(args.includes('--version')){console.log('codex-cli 0.154.0');process.exit(0);}
 if(args.includes('--help')){console.log('--json --output-schema --output-last-message --ignore-user-config --ignore-rules');process.exit(0);}
 if(args.includes('features')){console.log(['shell_tool','unified_exec','apps','plugins','hooks','view_image','skip_host_skill_discovery','multi_agent','code_mode_host','code_mode'].map(x=>x+' stable true').join('\\n'));process.exit(0);}
 if(args.includes('app-server')){
 if(behavior.metadataExit){process.stderr.write(behavior.metadataExit);process.exit(1);}
 const send=x=>process.stdout.write(JSON.stringify(x)+'\\n');
 readline.createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);if(r.id===undefined)return;
 if(r.method==='initialize')send({id:r.id,result:{userAgent:'test'}});
 else if(r.method==='config/read'){const config={};for(let i=0;i<args.length;i++){if(args[i]!=='-c')continue;const argument=args[++i],split=argument.indexOf('='),key=argument.slice(0,split);let value;try{value=JSON.parse(argument.slice(split+1));}catch{continue;}const parts=key.split('.');let target=config;for(const p of parts.slice(0,-1))target=target[p]??={};target[parts.at(-1)]=value;}if(behavior.unsafe)config.mcp_servers.unexpected={command:'bad'};send({id:r.id,result:{config}});}
 else if(r.method==='account/read'&&behavior.accountOffline)send({id:r.id,error:{message:'temporary metadata connection failure'}});
 else if(r.method==='account/read')send({id:r.id,result:{account:behavior.unauth?null:{type:behavior.api?'apiKey':'chatgpt',planType:'pro'}}});
 else if(r.method==='model/list'){
 if(behavior.offline)send({id:r.id,error:{message:'temporary service outage'}});
 else {const more=!r.params.cursor;send({id:r.id,result:{data:[{id:more?'provider-default':'second',model:more?'provider-default':'second',displayName:'Provider choice',isDefault:more,defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'medium'}]}],nextCursor:more?'page2':null}});}}
 });
 }else if(args.includes('exec')){
 let prompt='';process.stdin.setEncoding('utf8');process.stdin.on('data',s=>prompt+=s);process.stdin.on('end',()=>{
 fs.writeFileSync(root+'/invocation.json',JSON.stringify({args,prompt,cwd:process.cwd(),env:process.env}));
 const session=behavior.wrongSession?'other-session':'saved-session';console.log(JSON.stringify({type:'thread.started',thread_id:session}));
 if(behavior.fail){console.log(JSON.stringify({type:'turn.failed',error:{message:behavior.fail}}));process.exitCode=1;return;}
 console.log(JSON.stringify({type:'item.completed',item:{type:'mcp_tool_call',status:'completed'}}));
 const report=behavior.invalid?'nonsense':JSON.stringify({summary:'No actionable findings.',findings:[]});
 fs.writeFileSync(args[args.indexOf('--output-last-message')+1],report);
 console.log(JSON.stringify({type:'item.completed',item:{type:'agent_message',text:report}}));console.log(JSON.stringify({type:'turn.completed'}));
 });
 }
 `,
    { mode: 0o700 },
  );
  const worker = {
    codex,
    codexHome: join(root, "codex"),
    model: "provider-default",
    effort: "medium",
    subagents: { mode: "inherit", max: 8 },
  };
  const source = {
    dir: join(root, "source"),
    head: "a".repeat(40),
    base: "b".repeat(40),
    target: "main",
  };
  const job = {
    id: "job-one",
    repo: "owner/repo",
    number: 1,
    settings: worker,
    comparison: { head: source.head, base: source.base, target: "main" },
  };
  const set = (options) =>
    writeFile(join(root, "behavior.json"), JSON.stringify(options));
  return { root, worker, source, job, set };
}

test("model discovery follows pages and marks fallback catalog with a warning", async (t) => {
  const f = await fixture(t);
  const current = await discover(f.worker, f.root);
  assert.equal(current.models.length, 2);
  assert.equal(current.models[0].model, "provider-default");
  assert.equal(current.cached, false);
  await f.set({ offline: true });
  const old = await discover(f.worker, f.root);
  assert.equal(old.cached, true);
  assert.match(old.warning, /could not be retrieved.*cached/);
  assert.deepEqual(old.models, current.models);
});
test("discovery never invents choices or accepts API-key auth, including cached catalog", async (t) => {
  const f = await fixture(t, { offline: true });
  await assert.rejects(discover(f.worker, f.root), /no cached list/);
  await f.set({});
  await discover(f.worker, f.root);
  await f.set({ api: true });
  await assert.rejects(discover(f.worker, f.root), (e) => e.kind === "auth");
  assert.equal((await authStatus(f.worker, f.root)).authenticated, false);
});
test("review uses controlled cwd, fixed delegation settings, scrubbed env and persists report/session", async (t) => {
  const f = await fixture(t);
  const secretBefore = process.env.OPENAI_API_KEY;
  process.env.OPENAI_API_KEY = "should-not-leak";
  t.after(() => {
    if (secretBefore === undefined) delete process.env.OPENAI_API_KEY;
    else process.env.OPENAI_API_KEY = secretBefore;
  });
  f.job.settings.token = "worker-pairing-secret";
  const sessions = [],
    progress = [];
  const report = await runReview({
    ...f,
    guidance: { files: [{ path: "AGENTS.md", body: "Target guidance" }] },
    onSession: async (id) => sessions.push(id),
    onProgress: async (e) => progress.push(e),
  });
  assert.deepEqual(sessions, ["saved-session"]);
  assert.ok(progress.length);
  assert.equal(report.findings.length, 0);
  const invocation = JSON.parse(
    await readFile(join(f.root, "invocation.json"), "utf8"),
  );
  assert.equal(invocation.env.OPENAI_API_KEY, undefined);
  assert.equal(invocation.env.CODEX_HOME, f.worker.codexHome);
  assert.notEqual(invocation.env.HOME, process.env.HOME);
  assert.notEqual(invocation.cwd, f.source.dir);
  assert.ok(!invocation.args.includes("--ephemeral"));
  assert.ok(invocation.args.includes("features.shell_tool=false"));
  assert.ok(invocation.args.includes("features.unified_exec=false"));
  assert.ok(invocation.args.includes('forced_login_method="chatgpt"'));
  assert.match(invocation.prompt, /Target guidance/);
  const context = JSON.parse(
    await readFile(
      join(f.root, "reviews", f.job.id, "delegation-context.json"),
      "utf8",
    ),
  );
  assert.equal(context.job.settings.model, "provider-default");
  assert.equal(context.job.settings.effort, "medium");
  assert.deepEqual(context.job.settings.subagents, { mode: "inherit", max: 8 });
  assert.equal(context.job.settings.token, undefined);
  assert.ok(!JSON.stringify(context).includes("worker-pairing-secret"));
  assert.ok(invocation.args.includes("features.multi_agent=false"));
  assert.ok(invocation.args.includes("agents.enabled=false"));
  assert.equal(
    JSON.parse(
      await readFile(join(f.root, "reviews", "job-one", "session.json")),
    ).id,
    "saved-session",
  );
  assert.deepEqual(
    JSON.parse(
      await readFile(join(f.root, "reviews", "job-one", "report.json")),
    ),
    report,
  );
});
test("configured subagents use fixed delegation settings with native spawning disabled", async (t) => {
  const f = await fixture(t);
  f.job.settings.subagents = {
    mode: "configured",
    max: 3,
    model: "second",
    effort: "high",
  };
  const p = await prepareReview({ ...f, guidance: { files: [] } });
  assert.equal(p.config["agents.max_concurrent_threads_per_session"], 3);
  const context = JSON.parse(
    await readFile(join(p.dir, "delegation-context.json"), "utf8"),
  );
  assert.deepEqual(context.job.settings.subagents, {
    mode: "configured",
    max: 3,
    model: "second",
    effort: "high",
  });
  assert.equal(context.job.settings.model, "provider-default");
  assert.equal(p.config["features.multi_agent"], false);
  assert.equal(p.config["agents.enabled"], false);
});
test("failed response preserves session and resumes explicitly without repeating investigation", async (t) => {
  const f = await fixture(t, { invalid: true });
  await assert.rejects(runReview(f), (e) => e.kind === "output");
  await f.set({});
  f.job.session = "saved-session";
  await runReview(f);
  const invocation = JSON.parse(
    await readFile(join(f.root, "invocation.json")),
  );
  assert.equal(
    invocation.args[invocation.args.indexOf("resume") + 1],
    "saved-session",
  );
  assert.match(invocation.prompt, /Continue the incomplete/);
});
test("missing saved session and unexpected fresh thread require explicit restart", async (t) => {
  const f = await fixture(t);
  f.job.session = "missing";
  await assert.rejects(runReview(f), (e) => e.kind === "restart");
  delete f.job.session;
  await runReview(f);
  f.job.session = "saved-session";
  await f.set({ wrongSession: true });
  await assert.rejects(runReview(f), (e) => e.kind === "restart");
});
test("provider failures retain classification and provider-requested wait", async (t) => {
  const f = await fixture(t, { fail: "server unavailable; Retry-After: 45" });
  await assert.rejects(
    runReview(f),
    (e) => e.kind === "transient" && e.retryAfter === 45000,
  );
  assert.equal(classifyError(new Error("Usage limit exceeded")).kind, "quota");
  assert.equal(classifyError(new Error("refresh token expired")).kind, "auth");
  assert.equal(classifyError(new Error("thread not found")).kind, "restart");
});
test("unauthenticated review never starts inference and diagnostics performs no review", async (t) => {
  const f = await fixture(t, { unauth: true });
  assert.equal((await diagnostics(f.worker, f.root)).ok, true);
  await assert.rejects(runReview(f), (e) => e.kind === "auth");
  await assert.rejects(
    readFile(join(f.root, "invocation.json")),
    (e) => e.code === "ENOENT",
  );
});

test("unsupported parent or configured subagent model settings fail before inference", async (t) => {
  const f = await fixture(t);
  f.job.settings.model = "not-in-provider-catalog";
  await assert.rejects(runReview(f), (e) => e.kind === "config");
  await assert.rejects(
    readFile(join(f.root, "invocation.json")),
    (e) => e.code === "ENOENT",
  );
  f.job.settings.model = "provider-default";
  f.job.settings.subagents = {
    mode: "configured",
    max: 2,
    model: "second",
    effort: "unsupported-effort",
  };
  await assert.rejects(runReview(f), (e) => e.kind === "config");
  await assert.rejects(
    readFile(join(f.root, "invocation.json")),
    (e) => e.code === "ENOENT",
  );
});

test("locally persisted session reconciles a missed service callback before continuing", async (t) => {
  const f = await fixture(t, { fail: "server temporarily unavailable" });
  await assert.rejects(
    runReview({
      ...f,
      onSession: async () => {
        throw new Error("Service connection lost");
      },
    }),
  );
  assert.equal(
    JSON.parse(
      await readFile(join(f.root, "reviews", f.job.id, "session.json")),
    ).id,
    "saved-session",
  );
  assert.equal(f.job.session, undefined);
  await f.set({});
  const sessions = [];
  await runReview({ ...f, onSession: async (id) => sessions.push(id) });
  assert.equal(sessions[0], "saved-session");
  const invocation = JSON.parse(
    await readFile(join(f.root, "invocation.json")),
  );
  assert.equal(
    invocation.args[invocation.args.indexOf("resume") + 1],
    "saved-session",
  );
});

test("effective runtime policy rejects extra MCP servers before inference", async (t) => {
  const f = await fixture(t, { unsafe: true });
  await assert.rejects(runReview(f), (e) => e.kind === "config");
  await assert.rejects(
    readFile(join(f.root, "invocation.json")),
    (e) => e.code === "ENOENT",
  );
});

test("metadata failure keeps transient classification instead of demanding login", async (t) => {
  const f = await fixture(t, { accountOffline: true });
  const status = await authStatus(f.worker, f.root);
  assert.equal(status.errorKind, "transient");
  await assert.rejects(runReview(f), (e) => e.kind === "transient");
  await assert.rejects(
    readFile(join(f.root, "invocation.json")),
    (e) => e.code === "ENOENT",
  );
});

test("metadata process failures retain provider stderr", async (t) => {
  const f = await fixture(t, { metadataExit: "config mismatch\\n" });
  await assert.rejects(
    () => authStatus(f.worker, f.root),
    (error) => {
      assert.match(error.message, /Codex metadata connection exited \(1\)/);
      assert.match(error.message, /Codex stderr:\\nconfig mismatch/);
      return true;
    },
  );
});
