//! Opt-in installed-runtime probes. All model responses and credentials are synthetic.
//! Run: cargo test --test runtime_probe -- --ignored --test-threads=1
//! Set CROW_PROBE_CODEX to select an installed Codex binary. No live inference is used.
//! Set CROW_PROBE_FIXTURE_TOKEN=fixture to exercise delegated proxy env-key forwarding.
#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crow::provider::{self, Callbacks};
use futures_util::Stream;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const REPORT: &str = "Protocol fixture completed; no inference or code review was performed.";
const INVENTORY: &str = "text({fixtureTools:ALL_TOOLS.map(t=>t.name),fixtureGlobals:{process:typeof process,require:typeof require,fetch:typeof fetch,WebSocket:typeof WebSocket,Deno:typeof Deno,Bun:typeof Bun}});";
const START: &str = "const tool=ALL_TOOLS.find(t=>t.name.endsWith('__start_review_task')); text(await tools[tool.name]({task:'Verify and resume this bounded localhost fixture.'}));";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Inventory,
    Interrupt,
    Resume,
}
struct Capture {
    phase: Mode,
    child: bool,
    body: Value,
}
struct Shared {
    mode: Mode,
    main_calls: usize,
    child_calls: usize,
    captures: Vec<Capture>,
    held: Vec<(bool, Arc<AtomicBool>)>,
    errors: Vec<String>,
}
#[derive(Clone)]
struct Endpoint {
    shared: Arc<Mutex<Shared>>,
    root: PathBuf,
    expected_authorization: Option<&'static str>,
}

struct HeldStream {
    first: Option<Bytes>,
    closed: Arc<AtomicBool>,
}
impl Stream for HeldStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.first
            .take()
            .map_or(Poll::Pending, |v| Poll::Ready(Some(Ok(v))))
    }
}
impl Drop for HeldStream {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn sse(item: Value) -> Response {
    let events = [
        json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ];
    let text: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ([("content-type", "text/event-stream")], text).into_response()
}
fn complete() -> Response {
    sse(
        json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":json!({"summary":REPORT,"findings":[]}).to_string(),"annotations":[]}]}),
    )
}
fn tool(input: String, suffix: &str) -> Response {
    sse(
        json!({"type":"custom_tool_call","id":format!("ct_{suffix}"),"call_id":format!("call_{suffix}"),"name":"exec","namespace":"functions","input":input}),
    )
}
fn saved_task(root: &Path) -> Result<Option<Value>> {
    let directory = root.join("reviews/probe/tasks");
    let entries = match fs::read_dir(directory) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Ok(Some(serde_json::from_slice(&fs::read(path)?)?));
        }
    }
    Ok(None)
}
async fn until<T>(description: &str, mut check: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if let Some(value) = check()? {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .with_context(|| format!("Timed out: {description}"))?
}
async fn endpoint(
    State(endpoint): State<Endpoint>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let result = handle(&endpoint, &uri, &headers, bytes).await;
    match result {
        Ok(v) => v,
        Err(e) => {
            endpoint
                .shared
                .lock()
                .unwrap()
                .errors
                .push(format!("{e:#}"));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":{"message":e.to_string()}}).to_string(),
            )
                .into_response()
        }
    }
}
async fn handle(
    endpoint: &Endpoint,
    uri: &Uri,
    headers: &HeaderMap,
    bytes: Bytes,
) -> Result<Response> {
    if uri.path() != "/responses" {
        return Ok((
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"Local metadata fixture"}}).to_string(),
        )
            .into_response());
    }
    if let Some(expected) = endpoint.expected_authorization {
        ensure!(
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(expected),
            "Proxy request did not carry the synthetic fixture bearer token"
        );
    }
    let mut decoded = Vec::new();
    match headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
    {
        Some("gzip") => {
            flate2::read::GzDecoder::new(bytes.as_ref()).read_to_end(&mut decoded)?;
        }
        Some("zstd") => {
            decoded = zstd::stream::decode_all(bytes.as_ref())?;
        }
        None | Some("identity") => decoded.extend_from_slice(&bytes),
        other => bail!("Unsupported fixture request encoding: {other:?}"),
    }
    let body: Value = serde_json::from_slice(&decoded)?;
    let child = body["input"]
        .to_string()
        .contains("Your bounded delegated task:");
    let (phase, main_calls, child_calls) = {
        let mut state = endpoint.shared.lock().unwrap();
        if child {
            state.child_calls += 1;
        } else {
            state.main_calls += 1;
        }
        let phase = state.mode;
        state.captures.push(Capture { phase, child, body });
        (phase, state.main_calls, state.child_calls)
    };
    match phase {
        Mode::Inventory => {
            if child && child_calls == 1 {
                return Ok(tool(INVENTORY.into(), "child"));
            }
            if child {
                return Ok(complete());
            }
            if main_calls == 1 {
                return Ok(tool(format!("{INVENTORY}{START}"), "main"));
            }
        }
        Mode::Interrupt => {
            if !child && main_calls == 1 {
                return Ok(tool(START.into(), "start"));
            }
            let closed = Arc::new(AtomicBool::new(false));
            endpoint
                .shared
                .lock()
                .unwrap()
                .held
                .push((child, closed.clone()));
            let stream = HeldStream { first: Some(Bytes::from_static(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"held\",\"status\":\"in_progress\"}}\n\n")), closed };
            return Ok((
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response());
        }
        Mode::Resume => {
            if child {
                return Ok(complete());
            }
            if main_calls == 3 {
                let task = saved_task(&endpoint.root)?.context("Missing interrupted task")?;
                return Ok(tool(
                    format!(
                        "const tool=ALL_TOOLS.find(t=>t.name.endsWith('__resume_review_task')); text(await tools[tool.name]({{id:{}}}));",
                        task["id"]
                    ),
                    "resume",
                ));
            }
        }
    }
    until("delegated fixture completion", || {
        let task = saved_task(&endpoint.root)?;
        if let Some(task) = &task {
            ensure!(
                task["state"] != "paused",
                "Delegated fixture paused: {}",
                task["diagnostic"]
            );
        }
        Ok(task.filter(|v| v["state"] == "completed"))
    })
    .await?;
    Ok(complete())
}

struct Fixture {
    directory: tempfile::TempDir,
    endpoint: Endpoint,
    server: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    job: Value,
    source: Value,
    sessions: Arc<Mutex<Vec<Value>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server.abort();
    }
}
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
impl Fixture {
    async fn new(mode: Mode) -> Result<Self> {
        let proxy_env_key = match std::env::var("CROW_PROBE_FIXTURE_TOKEN") {
            Ok(value) if value == "fixture" => true,
            Err(std::env::VarError::NotPresent) => false,
            _ => bail!("CROW_PROBE_FIXTURE_TOKEN must be the synthetic value 'fixture'"),
        };
        let directory = tempfile::Builder::new()
            .prefix("crow-rust-runtime-probe-")
            .tempdir()?;
        let root = directory.path();
        let shared = Arc::new(Mutex::new(Shared {
            mode,
            main_calls: 0,
            child_calls: 0,
            captures: Vec::new(),
            held: Vec::new(),
            errors: Vec::new(),
        }));
        let endpoint = Endpoint {
            shared,
            root: root.to_owned(),
            expected_authorization: proxy_env_key.then_some("Bearer fixture"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let router = Router::new()
            .fallback(any(endpoint_handler))
            .with_state(endpoint.clone())
            .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        // An explicit synthetic routing file prevents discovery of the operator's personal configuration.
        let routing = root.join("fixture-routing.toml");
        let credentials = if proxy_env_key {
            "env_key = \"CROW_PROBE_FIXTURE_TOKEN\""
        } else {
            "experimental_bearer_token = \"fixture-only\""
        };
        fs::write(
            &routing,
            format!(
                "model_provider = \"fixture\"\n[model_providers.fixture]\nbase_url = \"{url}\"\nwire_api = \"responses\"\n{credentials}\n"
            ),
        )?;
        let codex_home = root.join("codex");
        fs::create_dir(&codex_home)?;
        let jwt = [json!({"alg":"none"}), json!({"sub":"fixture","aud":"fixture","iss":"fixture","exp":1999999999_u64,"email":"fixture@example.invalid","https://api.openai.com/auth":{"chatgpt_user_id":"fixture","chatgpt_account_id":"fixture","chatgpt_plan_type":"pro"}}), json!({"fixture":true})].iter().map(|v| URL_SAFE_NO_PAD.encode(v.to_string())).collect::<Vec<_>>().join(".");
        let auth = codex_home.join("auth.json");
        fs::write(&auth, json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":jwt,"refresh_token":"fixture-only","account_id":"fixture"},"last_refresh":chrono::Utc::now().to_rfc3339()}).to_string())?;
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600))?;
        let binary = std::env::var("CROW_PROBE_CODEX").unwrap_or_else(|_| "codex".into());
        let wrapper = root.join("codex-local-fixture");
        let transport = json!({"chatgpt_base_url":url,"model_providers.crow-probe.name":"Crow localhost fixture","model_providers.crow-probe.requires_openai_auth":true,"model_providers.crow-probe.wire_api":"responses","model_providers.crow-probe.base_url":url,"model_providers.crow-probe.supports_websockets":false,"model_providers.crow-probe.request_max_retries":0,"model_providers.crow-probe.stream_max_retries":0});
        let transport_args = transport
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("-c {}", shell_quote(&format!("{k}={v}"))))
            .collect::<Vec<_>>()
            .join(" ");
        let inspection_arg = shell_quote(&format!(
            "mcp_servers.crow_inspection.command={}",
            json!(
                std::env::var("CROW_TEST_BINARY")
                    .unwrap_or_else(|_| env!("CARGO_BIN_EXE_crow").to_owned())
            )
        ));
        // exec keeps the fixture-owned PID, allowing cancellation assertions without a wrapper child.
        let provider_name = if proxy_env_key {
            "crow_proxy"
        } else {
            "crow-probe"
        };
        let script = format!(
            "#!/bin/bash\nargs=()\nfor arg in \"$@\"; do\n case \"$arg\" in\n 'model_provider=\"openai\"'|'model_provider=\"crow_proxy\"') if [[ \" $* \" == *\" exec \"* ]]; then arg='model_provider=\"{provider_name}\"'; fi;;\n mcp_servers.crow_inspection.command=*) if [[ \" $* \" == *\" exec \"* ]]; then arg={inspection_arg}; fi;;\n esac\n args+=(\"$arg\")\ndone\nfor arg in \"${{args[@]}}\"; do\n if [[ \"$arg\" == exec ]]; then printf '%s\\n' \"$$\" >> {}; break; fi\ndone\nexec {} {transport_args} \"${{args[@]}}\"\n",
            shell_quote(&root.join("exec-pids").to_string_lossy()),
            shell_quote(&binary)
        );
        fs::write(&wrapper, script)?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
        let settings = json!({"codex":wrapper,"codexHome":codex_home,"codexProxy":{"configFile":routing,"baseUrl":url},"model":"gpt-6-astra","effort":"medium","subagents":{"mode":"configured","max":2,"model":"gpt-5.6-sol","effort":"low"},"retry":{"mode":"fixed","count":0,"delayMs":5000},"timeoutMs":30000});
        let comparison = json!({"head":"a".repeat(40),"base":"b".repeat(40),"target":"main"});
        let job = json!({"id":"probe","repo":"fixture/repository","number":1,"settings":settings,"comparison":comparison});
        let source =
            json!({"dir":root,"head":comparison["head"],"base":comparison["base"],"target":"main"});
        Ok(Self {
            directory,
            endpoint,
            server,
            cancel: CancellationToken::new(),
            job,
            source,
            sessions: Arc::new(Mutex::new(Vec::new())),
        })
    }
    fn callbacks(&self) -> Callbacks {
        let sessions = self.sessions.clone();
        Callbacks {
            on_session: Some(Arc::new(move |value| {
                let sessions = sessions.clone();
                Box::pin(async move {
                    sessions.lock().unwrap().push(value);
                    Ok(())
                })
            })),
            on_progress: None,
        }
    }
    async fn run(&self, job: &Value, cancel: CancellationToken) -> Result<Value> {
        provider::run_review(
            job,
            &self.source,
            &json!({"files":[]}),
            self.directory.path(),
            self.callbacks(),
            cancel,
        )
        .await
    }
    fn check_errors(&self) -> Result<()> {
        let state = self.endpoint.shared.lock().unwrap();
        ensure!(
            state.errors.is_empty(),
            "Local fixture errors: {:?}",
            state.errors
        );
        Ok(())
    }
}
// Keep a separate name because the fixture constructor also binds an Endpoint value.
async fn endpoint_handler(
    state: State<Endpoint>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    endpoint(state, uri, headers, bytes).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires installed Codex; synthetic loopback responses only"]
async fn runtime_tool_policy_and_delegation() -> Result<()> {
    let fixture = Fixture::new(Mode::Inventory).await?;
    let root = fixture.directory.path();
    let instruction = root.join("codex/AGENTS.md");
    fs::write(&instruction, "CROW_UNRELATED_INSTRUCTION_SENTINEL")?;
    let rejection = provider::provider_environment(&fixture.job["settings"], root)
        .err()
        .context("Global instruction guard accepted unrelated instructions")?;
    ensure!(provider::classify_error(&rejection).kind == "config");
    fs::remove_file(instruction)?;
    let result = fixture.run(&fixture.job, fixture.cancel.clone()).await?;
    fixture.check_errors()?;
    ensure!(result["summary"] == REPORT && result["findings"] == json!([]));
    let state = fixture.endpoint.shared.lock().unwrap();
    let child = state
        .captures
        .iter()
        .find(|c| c.child)
        .context("Missing child request")?;
    ensure!(
        child.body["model"] == "gpt-5.6-sol" && child.body["reasoning"]["effort"] == "low",
        "Delegated model or reasoning was not enforced"
    );
    let allowed = [
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
    ];
    let mut seen = [false; 2];
    for capture in &state.captures {
        ensure!(capture.body["text"]["format"]["type"] == "json_schema");
        let input = capture.body["input"]
            .as_array()
            .context("Request input is not an array")?;
        ensure!(
            !capture
                .body
                .to_string()
                .contains("CROW_UNRELATED_INSTRUCTION_SENTINEL")
        );
        ensure!(
            !capture.body["instructions"]
                .to_string()
                .contains("Available skills"),
            "Unrelated skills leaked into prompt"
        );
        for output in input
            .iter()
            .filter(|v| v["type"] == "custom_tool_call_output")
        {
            let text = output["output"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| output["output"].to_string());
            let output: Value = serde_json::from_str(&text)?;
            let blocks = output
                .as_array()
                .or_else(|| output["content"].as_array())
                .context("Missing Code Mode output blocks")?;
            let found = blocks
                .iter()
                .filter_map(|b| serde_json::from_str::<Value>(b["text"].as_str()?).ok())
                .find(|v| v["fixtureTools"].is_array());
            let Some(found) = found else {
                continue;
            };
            seen[usize::from(capture.child)] = true;
            let globals = found["fixtureGlobals"]
                .as_object()
                .context("Missing global inventory")?;
            ensure!(
                globals.len() == 6 && globals.values().all(|v| v == "undefined"),
                "Code Mode exposes a host or network global: {globals:?}"
            );
            let tools = found["fixtureTools"].as_array().unwrap();
            for required in ["list_files", "read_file", "search", "diff"] {
                ensure!(
                    tools.contains(&json!(format!("mcp__crow_inspection__{required}"))),
                    "Missing inspection tool: {required}"
                );
            }
            ensure!(
                capture.child || tools.contains(&json!("mcp__crow_inspection__start_review_task")),
                "Parent cannot delegate through Crow"
            );
            for name in tools {
                let name = name.as_str().context("Non-string tool name")?;
                ensure!(allowed.contains(&name), "Unexpected nested tool: {name}");
                ensure!(
                    !capture.child || !name.ends_with("start_review_task"),
                    "Child can bypass delegation ceiling"
                );
            }
        }
    }
    ensure!(
        seen == [true, true],
        "Missing actual parent or child tool inventory: {seen:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires installed Codex; synthetic interruption and resume only"]
async fn interruption_stops_processes_and_resumes_parent_and_child() -> Result<()> {
    let fixture = Fixture::new(Mode::Interrupt).await?;
    let root = fixture.directory.path();
    let cancel = fixture.cancel.child_token();
    let initial = fixture.run(&fixture.job, cancel.clone());
    tokio::pin!(initial);
    let ready = until("parent and child streams and saved sessions", || {
        fixture.check_errors()?;
        let state = fixture.endpoint.shared.lock().unwrap();
        let held = state.held.iter().any(|(child, _)| *child)
            && state.held.iter().any(|(child, _)| !*child);
        let task = saved_task(root)?;
        if let Some(task) = &task {
            ensure!(
                task["state"] != "paused",
                "Delegated fixture paused before interruption: {}",
                task["diagnostic"]
            );
        }
        Ok(task.filter(|v| {
            held && v["session"].is_string() && !fixture.sessions.lock().unwrap().is_empty()
        }))
    });
    let before = tokio::select! { result = &mut initial => bail!("Review ended before interruption: {result:?}"), result = ready => result? };
    let parent_session = fixture.sessions.lock().unwrap()[0].clone();
    cancel.cancel();
    let interruption = initial
        .await
        .err()
        .context("Interrupted review unexpectedly completed")?;
    ensure!(
        provider::classify_error(&interruption).kind == "interrupted"
            || interruption.to_string().contains("Interrupted"),
        "Unexpected interruption error: {interruption:#}"
    );
    until("interrupted streams closing", || {
        Ok(fixture
            .endpoint
            .shared
            .lock()
            .unwrap()
            .held
            .iter()
            .all(|(_, closed)| closed.load(Ordering::SeqCst))
            .then_some(()))
    })
    .await?;
    let paused = until("durable paused child", || {
        Ok(saved_task(root)?.filter(|v| v["state"] == "paused"))
    })
    .await?;
    ensure!(paused["session"] == before["session"]);
    let pids = fs::read_to_string(root.join("exec-pids"))?
        .lines()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        pids.len() == 2,
        "Expected parent and child exec processes, got {pids:?}"
    );
    for pid in pids {
        until(&format!("interrupted process {pid} stopping"), || {
            #[cfg(target_os = "linux")]
            {
                match fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Ok(stat) => Ok(stat.contains(") Z ").then_some(())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(())),
                    Err(e) => Err(e.into()),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok((unsafe { libc::kill(pid as i32, 0) } != 0).then_some(()))
            }
        })
        .await?;
    }
    fixture.endpoint.shared.lock().unwrap().mode = Mode::Resume;
    let mut resumed = fixture.job.clone();
    resumed["session"] = parent_session.clone();
    resumed["resumeEpoch"] = json!(1);
    let result = fixture.run(&resumed, fixture.cancel.child_token()).await?;
    fixture.check_errors()?;
    ensure!(result["summary"] == REPORT && result["findings"] == json!([]));
    ensure!(fixture.sessions.lock().unwrap().last() == Some(&parent_session));
    let task = saved_task(root)?.context("Missing resumed child")?;
    ensure!(task["state"] == "completed" && task["session"] == before["session"]);
    let events_path = root
        .join("reviews/probe/tasks")
        .join(before["id"].as_str().unwrap())
        .join("runtime/events.jsonl");
    let events = fs::read_to_string(events_path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let starts: Vec<_> = events
        .iter()
        .filter(|v| v["type"] == "thread.started")
        .collect();
    ensure!(
        starts.len() == 2 && starts.iter().all(|v| v["thread_id"] == before["session"]),
        "Child resume did not retain original session"
    );
    let state = fixture.endpoint.shared.lock().unwrap();
    ensure!(state.child_calls == 2);
    ensure!(
        state
            .captures
            .iter()
            .any(|c| c.phase == Mode::Resume && c.child)
    );
    for capture in &state.captures {
        ensure!(
            capture.body["text"]["format"]["type"] == "json_schema",
            "Missing output schema on initial or resumed request"
        );
    }
    Ok(())
}
