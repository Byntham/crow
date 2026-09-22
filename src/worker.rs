//! Lease-aware review worker. Completed inference is persisted before publication.
use crate::{
    provider::{self, Callbacks},
    retention::{valid_id, valid_session},
    util,
};
use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const INVALID: &str = "Crow service returned an invalid worker response";
fn string(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| anyhow::anyhow!(INVALID))
}
fn object(v: &Value) -> Result<()> {
    ensure!(v.is_object(), INVALID);
    Ok(())
}
fn number(v: &Value) -> Result<()> {
    ensure!(v.as_f64().is_some_and(f64::is_finite), INVALID);
    Ok(())
}
fn state(v: &Value) -> Result<()> {
    ensure!(
        matches!(
            v.as_str(),
            Some(
                "queued"
                    | "held"
                    | "reviewing"
                    | "retrying"
                    | "paused"
                    | "publishing"
                    | "completed"
                    | "superseded"
                    | "cancelled"
            )
        ),
        INVALID
    );
    Ok(())
}
fn session(v: &Value) -> Result<()> {
    ensure!(
        v.is_null()
            || v.as_str()
                .is_some_and(|s| valid_session(s) && s.bytes().all(|b| !b.is_ascii_uppercase())),
        INVALID
    );
    Ok(())
}
/// Validate every service field consumed by the worker before touching its filesystem.
pub fn validate_response(config: &Value, action: &str, mut value: Value) -> Result<Value> {
    match action {
        "next" => {
            if value.is_null() {
                return Ok(value);
            }
            object(&value)?;
            string(&value["token"])?;
            let job = &mut value["job"];
            object(job)?;
            for k in ["id", "key", "repo", "head", "target", "worker"] {
                string(&job[k])?;
            }
            ensure!(valid_id(string(&job["id"])?), INVALID);
            for k in [
                "number",
                "priority",
                "resumeEpoch",
                "createdAt",
                "updatedAt",
                "retries",
                "nextAt",
            ] {
                number(&job[k])?;
            }
            for k in ["manual", "restart"] {
                ensure!(job[k].is_boolean(), INVALID);
            }
            state(&job["state"])?;
            session(&job["session"])?;
            for k in [
                "patch",
                "lease",
                "author",
                "reviewUrl",
                "guidanceFingerprint",
                "guidanceTargetSha",
                "parentId",
                "taskId",
                "trigger",
            ] {
                if let Some(v) = job.get(k) {
                    string(v)?;
                }
            }
            for k in ["startedAt", "publishAt"] {
                if let Some(v) = job.get(k) {
                    number(v)?;
                }
            }
            if let Some(v) = job.get("reason").filter(|v| !v.is_null()) {
                string(v)?;
            }
            if let Some(v) = job.get("autoRecover") {
                ensure!(v.is_boolean(), INVALID);
            }
            if let Some(v) = job.get("comparison") {
                object(v)?;
                for k in ["head", "base", "target", "targetSha"] {
                    string(&v[k])?;
                }
            }
            if let Some(v) = job.get("prContext") {
                object(v)?;
                string(&v["title"])?;
                string(&v["body"])?;
            }
            if let Some(v) = job.get("settingsChanges") {
                for c in v.as_array().ok_or_else(|| anyhow::anyhow!(INVALID))? {
                    object(c)?;
                    for k in ["model", "effort"] {
                        if !c[k].is_null() {
                            string(&c[k])?;
                        } else {
                            ensure!(c.get(k).is_some(), INVALID);
                        }
                    }
                    number(&c["at"])?;
                }
            }
            if let Some(v) = job.get("settings") {
                object(v)?;
                for k in ["model", "effort", "subagents", "retry", "timeoutMs"] {
                    ensure!(v.get(k).is_some(), INVALID);
                }
                let mut merged = config.clone();
                let worker = merged["worker"]
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!(INVALID))?;
                worker.extend(v.as_object().unwrap().clone());
                crate::config::validate_config(&merged)?;
            }
            if !job["report"].is_null() {
                job["report"] = crate::report::validate_report(&job["report"])?;
            }
            let pr = &value["pr"];
            object(pr)?;
            number(&pr["number"])?;
            string(&pr["state"])?;
            ensure!(pr["draft"].is_boolean(), INVALID);
            string(&pr["user"]["login"])?;
            string(&pr["head"]["sha"])?;
            string(&pr["base"]["sha"])?;
            string(&pr["base"]["ref"])?;
            for k in ["title", "updated_at"] {
                if let Some(v) = pr.get(k) {
                    string(v)?;
                }
            }
            if let Some(v) = pr.get("body").filter(|v| !v.is_null()) {
                string(v)?;
            }
        }
        "maintenance" => {
            object(&value)?;
            number(&value["retentionDays"])?;
            for job in value["jobs"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!(INVALID))?
            {
                object(job)?;
                ensure!(valid_id(string(&job["id"])?), INVALID);
                state(&job["state"])?;
                number(&job["updatedAt"])?;
                session(&job["session"])?;
            }
        }
        "ping" => {
            object(&value)?;
            ensure!(value["ok"].is_boolean(), INVALID);
            string(&value["id"])?;
        }
        "heartbeat" | "comparison" | "session" | "progress" | "report" | "failed" => {
            object(&value)?;
            for k in ["cancel", "skip", "ok"] {
                if let Some(v) = value.get(k) {
                    ensure!(v.is_boolean(), INVALID);
                }
            }
        }
        _ => bail!("Unknown worker action: {action}"),
    }
    crate::config::normalize_numbers(&mut value);
    Ok(value)
}
async fn request(
    client: &reqwest::Client,
    config: &Value,
    action: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    let url = format!(
        "{}/worker/{action}",
        string(&config["serviceUrl"])?.trim_end_matches('/')
    );
    let send = async {
        let response = client
            .post(url)
            .bearer_auth(string(&config["worker"]["token"])?)
            .json(args)
            .timeout(Duration::from_secs(35))
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        if !status.is_success() {
            bail!(
                "{}",
                value["error"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Crow service returned {status}"))
            );
        }
        validate_response(config, action, value)
    };
    tokio::select! { result=send=>result, _=cancel.cancelled()=>bail!("Worker request cancelled") }
}
pub async fn worker_request(config: &Value, action: &str, args: &Value) -> Result<Value> {
    request(
        &reqwest::Client::new(),
        config,
        action,
        args,
        CancellationToken::new(),
    )
    .await
}
#[async_trait]
pub trait WorkerBackend: Send + Sync {
    async fn request(
        &self,
        config: &Value,
        action: &str,
        body: &Value,
        cancel: CancellationToken,
    ) -> Result<Value>;
    async fn checkout(
        &self,
        root: &Path,
        job: &Value,
        pr: &Value,
        token: &str,
        cancel: CancellationToken,
    ) -> Result<Value>;
    async fn guidance(&self, source: &Value) -> Result<Value>;
    async fn patch(
        &self,
        source: &Value,
        findings: &Value,
        cancel: CancellationToken,
    ) -> Result<String>;
    async fn review(
        &self,
        root: &Path,
        job: &Value,
        source: &Value,
        guidance: &Value,
        callbacks: Callbacks,
        cancel: CancellationToken,
    ) -> Result<Value>;
}
struct NativeBackend {
    client: reqwest::Client,
}
#[async_trait]
impl WorkerBackend for NativeBackend {
    async fn request(&self, c: &Value, a: &str, b: &Value, t: CancellationToken) -> Result<Value> {
        request(&self.client, c, a, b, t).await
    }
    async fn checkout(
        &self,
        r: &Path,
        j: &Value,
        p: &Value,
        t: &str,
        c: CancellationToken,
    ) -> Result<Value> {
        crate::inspection::checkout(r, j, p, t, c).await
    }
    async fn guidance(&self, s: &Value) -> Result<Value> {
        crate::inspection::guidance(s).await
    }
    async fn patch(&self, s: &Value, f: &Value, c: CancellationToken) -> Result<String> {
        crate::inspection::publication_patch(s, f, c).await
    }
    async fn review(
        &self,
        r: &Path,
        j: &Value,
        s: &Value,
        g: &Value,
        b: Callbacks,
        c: CancellationToken,
    ) -> Result<Value> {
        provider::run_review(j, s, g, r, b, c).await
    }
}
#[derive(Clone)]
pub struct WorkerOptions {
    pub heartbeat: Duration,
    pub poll: Duration,
    pub reconnect: Duration,
    pub maintenance: Duration,
    pub status: Duration,
}
impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(10),
            poll: Duration::from_secs(1),
            reconnect: Duration::from_secs(5),
            maintenance: Duration::from_secs(3600),
            status: Duration::from_secs(30),
        }
    }
}
struct Active {
    cancel: CancellationToken,
    repo: Value,
    number: Value,
}
struct State {
    active: BTreeMap<String, Active>,
    sessions: BTreeMap<String, String>,
    claiming: bool,
    draining: bool,
    closed: bool,
    connection: &'static str,
}
struct Shared {
    config: Value,
    root: PathBuf,
    backend: Arc<dyn WorkerBackend>,
    options: WorkerOptions,
    state: Mutex<State>,
    changed: Notify,
    wake: Notify,
    maintenance_requested: Notify,
    stop: CancellationToken,
}
pub struct WorkerHandle {
    shared: Arc<Shared>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}
fn saved_sessions(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let path = root.join("reviews");
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => (),
        Ok(_) => return Ok(result),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e.into()),
    }
    for entry in fs::read_dir(path)?.take(10_000) {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || !valid_id(&id) {
            continue;
        }
        let path = entry.path().join("session.json");
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= 1_048_576) {
            continue;
        }
        if let Ok(Some(v)) = util::read_json(&path)
            && let Some(s) = v["id"]
                .as_str()
                .filter(|s| valid_session(s) && s.bytes().all(|b| !b.is_ascii_uppercase()))
        {
            result.insert(id, s.into());
        }
    }
    Ok(result)
}
impl Shared {
    async fn status(&self) {
        let s = self.state.lock().await;
        let active:Vec<Value>=s.active.iter().map(|(id,a)|json!({"id":id,"repo":a.repo,"number":a.number,"state":if a.cancel.is_cancelled(){"stopping"}else{"reviewing"}})).collect();
        let value = json!({"version":1,"pid":std::process::id(),"updatedAt":util::now(),"state":if s.closed{"stopped"}else if s.draining{"draining"}else{"running"},"connection":if s.closed{"stopped"}else{s.connection},"active":active});
        if util::atomic(&self.root.join("worker-status.json"), &value).is_err() {
            eprintln!("Worker status could not be saved; inspect local file permissions.");
        }
    }
    async fn rpc(&self, action: &str, body: &Value, cancel: CancellationToken) -> Result<Value> {
        let result = self
            .backend
            .request(&self.config, action, body, cancel)
            .await;
        let connection = if result.is_ok() {
            "connected"
        } else {
            "unavailable"
        };
        let changed = {
            let mut state = self.state.lock().await;
            let changed = !state.closed && state.connection != connection;
            if changed {
                state.connection = connection;
            }
            changed
        };
        if changed {
            self.status().await;
        }
        result
    }
    async fn send(&self, job: &Value, action: &str, body: Value) -> Result<Value> {
        let mut value = json!({"id":job["id"],"lease":job["lease"]});
        if let Some(obj) = body.as_object() {
            value.as_object_mut().unwrap().extend(obj.clone());
        }
        self.rpc(action, &value, CancellationToken::new()).await
    }
    fn log(&self, job: &Value, error: &str, token: &str) -> Result<()> {
        let mut message = error.to_owned();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        for secret in [
            self.config["worker"]["token"].as_str().unwrap_or(""),
            self.config["adminToken"].as_str().unwrap_or(""),
            token,
            if token.is_empty() { "" } else { &encoded },
        ] {
            if !secret.is_empty() {
                message = message.replace(secret, "[redacted]");
            }
        }
        let tokens = regex::Regex::new(r"(?:gh[psuor]_\w+|github_pat_\w+|sk-[\w-]+)")?;
        message = tokens.replace_all(&message, "[redacted]").into_owned();
        let auth = regex::Regex::new(r"(?i)(Authorization\s*[:=]\s*(?:Bearer|Basic)\s+)\S+")?;
        message = auth.replace_all(&message, "${1}[redacted]").into_owned();
        let path = self
            .root
            .join("logs")
            .join(format!("{}.log", string(&job["id"])?));
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        writeln!(
            options.open(path)?,
            "{} {}",
            chrono::Utc::now().to_rfc3339(),
            message
        )?;
        Ok(())
    }
}
fn cancelled(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Review interrupted");
    Ok(())
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Restart(String);
async fn execute(shared: Arc<Shared>, work: Value, cancel: CancellationToken) {
    let mut job = work["job"].clone();
    let mut settings = shared.config["worker"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    settings.remove("id");
    settings.remove("token");
    if let Some(extra) = job["settings"].as_object() {
        settings.extend(extra.clone());
    }
    // Execution authority is local to this worker, never supplied by a job.
    settings.insert(
        "execution".into(),
        shared.config["worker"]["execution"].clone(),
    );
    job["settings"] = Value::Object(settings);
    job["prContext"] = json!({"title":work["pr"]["title"].as_str().unwrap_or("").chars().take(1000).collect::<String>(),"body":work["pr"]["body"].as_str().unwrap_or("").chars().take(16000).collect::<String>()});
    let session_value = Arc::new(Mutex::new(job["session"].clone()));
    let heartbeat_stop = CancellationToken::new();
    let _heartbeat_guard = heartbeat_stop.clone().drop_guard();
    let heartbeat = {
        let shared = shared.clone();
        let job = job.clone();
        let cancel = cancel.clone();
        let stop = heartbeat_stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(shared.options.heartbeat)=>()}
                if cancel.is_cancelled() {
                    continue;
                }
                let mut body = json!({});
                if let Ok(progress) = crate::runtime_status::Progress::read(&shared.root, &job) {
                    body["runtime"] = json!(progress);
                }
                match shared.send(&job, "heartbeat", body).await {
                    Ok(v) if v["cancel"] != true => (),
                    _ => {
                        cancel.cancel();
                        shared.status().await;
                    }
                }
            }
        })
    };
    let result:Result<()>=async {
        cancelled(&cancel)?;
        let source=shared.backend.checkout(&shared.root,&job,&work["pr"],work["token"].as_str().unwrap_or(""),cancel.clone()).await?;
        cancelled(&cancel)?;
        let rules_file=shared.root.join("reviews").join(string(&job["id"])?).join("guidance.json");
        let mut rules;
        if let Some(fingerprint)=job["guidanceFingerprint"].as_str().filter(|s|!s.is_empty()) {
            if let Some(saved)=util::read_json(&rules_file).ok().flatten(){rules=saved;}
            else {
                let target=job["guidanceTargetSha"].as_str().or_else(||job["comparison"]["targetSha"].as_str()).ok_or_else(||Restart("Original review guidance revision is unavailable. Restart required.".into()))?;
                let mut pinned=source.clone();pinned["targetSha"]=json!(target);
                rules=shared.backend.guidance(&pinned).await.map_err(|_|Restart("Original review guidance cannot be reconstructed. Restart required.".into()))?;
                rules["targetSha"]=json!(target);
            }
            if rules["fingerprint"]!=fingerprint || util::hash(&rules["files"])!=fingerprint {return Err(Restart("Saved review guidance does not match the original review. Restart required.".into()).into());}
        } else {rules=shared.backend.guidance(&source).await?;rules["targetSha"]=source["targetSha"].clone();}
        util::atomic(&rules_file,&rules)?;cancelled(&cancel)?;
        job["comparison"]=source.clone();job["guidanceFingerprint"]=rules["fingerprint"].clone();job["guidanceTargetSha"]=rules["targetSha"].clone();
        let decision=shared.send(&job,"comparison",json!({"comparison":source,"guidanceFingerprint":rules["fingerprint"],"guidanceTargetSha":rules["targetSha"]})).await?;
        if decision["cancel"]==true || decision["skip"]==true{return Ok(());}
        cancelled(&cancel)?;
        let local=shared.root.join("reports").join(format!("{}.json",string(&job["id"])?));
        let saved=util::read_json(&local).ok().flatten();
        let mut report=job["report"].clone();
        if report.is_null()&& let Some(saved)=saved&& ["head","base","target"].iter().all(|k|saved[k]==source[k]){report=saved["report"].clone();}
        if report.is_null(){
            let callbacks=Callbacks{
                on_session:Some({let shared=shared.clone();let job=job.clone();let cancel=cancel.clone();let session_value=session_value.clone();Arc::new(move |value:Value| {let shared=shared.clone();let job=job.clone();let cancel=cancel.clone();let session_value=session_value.clone();Box::pin(async move {
                    let session=string(&value)?.to_owned(); *session_value.lock().await=json!(session);
                    if valid_session(&session){shared.state.lock().await.sessions.insert(string(&job["id"] )?.to_owned(),session.clone());}
                    let result=shared.send(&job,"session",json!({"session":session})).await?;
                    if result["cancel"]==true{cancel.cancel();shared.status().await;}
                    Ok(())
                })})}),
                on_progress:Some({let shared=shared.clone();let job=job.clone();let cancel=cancel.clone();let token=work["token"].as_str().unwrap_or("").to_owned();Arc::new(move |value:Value|{let shared=shared.clone();let job=job.clone();let cancel=cancel.clone();let token=token.clone();Box::pin(async move {
                    if value["type"]=="warning" {eprintln!("The provider model list could not be refreshed; Crow is using its cached catalog. Run crow models for details.");shared.log(&job,value["message"].as_str().unwrap_or("Provider warning"),&token)?;return Ok(());}
                    let result=shared.send(&job,"progress",json!({})).await?;
                    if result["cancel"]==true{cancel.cancel();shared.status().await;}
                    Ok(())
                })})}),
            };
            report=shared.backend.review(&shared.root,&job,&source,&rules,callbacks,cancel.clone()).await?;
        }
        report=crate::report::validate_report(&report)?;
        // A provider completion racing cancellation is still useful for the next lease.
        util::atomic(&local,&json!({"head":source["head"],"base":source["base"],"target":source["target"],"report":report}))?;
        cancelled(&cancel)?;
        let patch=shared.backend.patch(&source,&report["findings"],cancel.clone()).await?;
        cancelled(&cancel)?;
        let runtime=crate::runtime_status::Progress::read(&shared.root,&job).ok();
        let acknowledgement=shared.send(&job,"report",json!({"report":report,"patch":patch,"runtime":runtime})).await?;
        // A successful RPC may reject this lease because the review was paused
        // or its lease changed while the report was in flight. Preserve resumable work.
        if acknowledgement["cancel"]==true {cancel.cancel();return Ok(());}
        ensure!(acknowledgement["ok"]==true,"The service did not confirm report acceptance");
        // The validated report is durable locally and accepted by the service.
        // Publishing retries need evidence, not the large prepared workspaces.
        let root = shared.root.clone(); let mut finished = job.clone(); finished["state"] = json!("completed");
        let cleanup = tokio::task::spawn_blocking(move || crate::retention::cleanup_runtime(&root, &[finished])).await;
        let cleanup = match cleanup { Ok(Ok(result)) => result, other => json!({"warnings":[format!("Prepared environment cleanup failed: {other:?}")]}) };
        let _ = util::atomic(&shared.root.join("reviews").join(string(&job["id"])?).join("runtime-cleanup.json"), &cleanup);
        if cleanup["warnings"].as_array().is_some_and(|warnings| !warnings.is_empty()) { eprintln!("Prepared environment cleanup needs attention; inspect the review's runtime-cleanup.json."); }
        Ok(())
    }.await;
    if let Err(error) = result {
        let _ = shared.log(
            &job,
            &format!("{error:#}"),
            work["token"].as_str().unwrap_or(""),
        );
        eprintln!(
            "Review {}#{} interrupted; see local logs.",
            job["repo"].as_str().unwrap_or(""),
            job["number"]
        );
        let provider_error = error.downcast_ref::<provider::ProviderError>();
        let kind = if cancel.is_cancelled() {
            "interrupted"
        } else if error.downcast_ref::<Restart>().is_some() {
            "restart"
        } else if let Some(error) = error.downcast_ref::<crate::inspection::InspectionError>() {
            &error.kind
        } else if error.downcast_ref::<crate::report::OutputError>().is_some() {
            "output"
        } else if let Some(error) = provider_error {
            &error.kind
        } else {
            "transient"
        };
        let mut body =
            json!({"kind":kind,"retryAfter":provider_error.map_or(0, |e| e.retry_after)});
        if let Some(id) = session_value
            .lock()
            .await
            .as_str()
            .filter(|s| valid_session(s))
        {
            body["session"] = json!(id);
        }
        if let Ok(progress) = crate::runtime_status::Progress::read(&shared.root, &job) {
            body["runtime"] = json!(progress);
        }
        let _ = shared.send(&job, "failed", body).await;
    }
    heartbeat_stop.cancel();
    let _ = heartbeat.await;
}
async fn run_loop(shared: Arc<Shared>) {
    let mut defaults = Map::new();
    for k in [
        "model",
        "effort",
        "subagents",
        "retry",
        "timeoutMs",
        "concurrency",
    ] {
        defaults.insert(k.into(), shared.config["worker"][k].clone());
    }
    loop {
        let claim = {
            let mut s = shared.state.lock().await;
            if s.closed {
                break;
            }
            if !s.draining
                && s.active.len()
                    < shared.config["worker"]["concurrency"].as_u64().unwrap_or(1) as usize
            {
                s.claiming = true;
                Some(
                    json!({"defaults":defaults,"active":s.active.keys().collect::<Vec<_>>(),"sessions":s.sessions}),
                )
            } else {
                None
            }
        };
        let mut delay = shared.options.poll;
        if let Some(body) = claim {
            let result = shared.rpc("next", &body, shared.stop.clone()).await;
            let mut s = shared.state.lock().await;
            match result {
                Ok(work) if !work.is_null() => {
                    let id = work["job"]["id"].as_str().unwrap_or("").to_owned();
                    if valid_id(&id) && !s.active.contains_key(&id) {
                        let cancel = shared.stop.child_token();
                        if s.closed {
                            cancel.cancel();
                        }
                        s.active.insert(
                            id.clone(),
                            Active {
                                cancel: cancel.clone(),
                                repo: work["job"]["repo"].clone(),
                                number: work["job"]["number"].clone(),
                            },
                        );
                        let task_shared = shared.clone();
                        tokio::spawn(async move {
                            if tokio::spawn(execute(task_shared.clone(), work, cancel))
                                .await
                                .is_err()
                            {
                                eprintln!(
                                    "Review task stopped unexpectedly; its lease will be recovered by the service."
                                );
                            }
                            task_shared.state.lock().await.active.remove(&id);
                            task_shared.changed.notify_waiters();
                            task_shared.status().await;
                            task_shared.maintenance_requested.notify_one();
                        });
                        delay = Duration::ZERO;
                    } else {
                        eprintln!("Service returned an invalid or already active review job.");
                        delay = shared.options.reconnect;
                    }
                }
                Ok(_) => (),
                Err(_) => {
                    if !s.closed {
                        eprintln!("Worker connection unavailable; reconnecting.");
                    }
                    delay = shared.options.reconnect;
                }
            }
            s.claiming = false;
            drop(s);
            shared.changed.notify_waiters();
            shared.status().await;
        }
        tokio::select! {
            _ = shared.stop.cancelled() => break,
            _ = shared.wake.notified() => (),
            _ = tokio::time::sleep(delay) => (),
        }
    }
}
/// Cleanup for the worker loop and `crow cleanup`. Ownership receipts survive failures.
pub async fn cleanup_runtime_data(
    root: &Path,
    execution: &Value,
    jobs: &[Value],
    days: &Value,
) -> Result<Value> {
    let Some(_lock) = maintenance_lock(root)? else {
        return Ok(
            json!({"skipped":true,"reason":"Another runtime maintenance pass is running.","previous":runtime_maintenance_status(root)}),
        );
    };
    let cache = prune_runtime_cache(root).await;
    cleanup_runtime_data_locked(root, execution, jobs, days, cache).await
}
fn maintenance_lock(root: &Path) -> Result<Option<crate::retention::Lock>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("runtime-maintenance.lock"))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(crate::retention::Lock(file))),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub fn runtime_maintenance_status(root: &Path) -> Value {
    match util::read_json(&root.join("runtime-maintenance.json")) {
        Ok(Some(value)) if value.is_object() => value,
        Ok(None) => Value::Null,
        other => {
            json!({"warningCount":1,"warnings":[crate::runtime_diagnostics::bounded_error(format!("Could not read runtime-maintenance.json: {other:?}"))]})
        }
    }
}
async fn prune_runtime_cache(root: &Path) -> Result<u64> {
    let cache_root = root.join("runtime-cache");
    tokio::task::spawn_blocking(move || crate::runtime_cache::maintain(&cache_root)).await?
}
async fn cleanup_runtime_data_locked(
    root: &Path,
    execution: &Value,
    jobs: &[Value],
    days: &Value,
    cache: Result<u64>,
) -> Result<Value> {
    let mut warnings = Vec::new();
    let cache_bytes = match cache {
        Ok(bytes) => bytes,
        Err(error) => {
            warnings.push(format!("Package cache cleanup: {error:#}"));
            0
        }
    };
    let executable = execution["podman"].as_str().unwrap_or("podman");
    let env = util::host_env().into_iter().collect();
    let configured_images: Vec<_> = execution["repositories"]
        .as_object()
        .into_iter()
        .flat_map(|repositories| repositories.values())
        .filter_map(|policy| policy["image"].as_str())
        .map(str::to_owned)
        .collect();
    let runtime = match crate::runtime_cleanup::cleanup_with_images(
        root,
        executable,
        &env,
        jobs,
        &configured_images,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            warnings.push(format!("Runtime resource cleanup: {error:#}"));
            json!({"deferredJobs":jobs.iter().map(|job| job["id"].clone()).collect::<Vec<_>>()})
        }
    };
    let deferred = runtime["deferredJobs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let retained: Vec<_> = jobs
        .iter()
        .map(|job| {
            let mut job = job.clone();
            if deferred.contains(&job["id"]) {
                job["state"] = json!("paused");
            }
            job
        })
        .collect();
    let root_path = root.to_owned();
    let config = json!({"retentionDays":days});
    let retention = tokio::task::spawn_blocking(move || {
        crate::retention::cleanup(&root_path, &config, &retained)
    })
    .await??;
    for result in [&runtime, &retention] {
        for warning in result["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            warnings.push(warning.to_owned());
        }
    }
    let warning_count = warnings.len();
    let warnings: Vec<_> = warnings
        .into_iter()
        .take(100)
        .map(crate::runtime_diagnostics::bounded_error)
        .collect();
    let result = json!({"updatedAt":util::now(),"cacheBytesRemoved":cache_bytes,"runtime":runtime,"removed":retention["removed"],"retention":retention,"warningCount":warning_count,"warnings":warnings});
    util::atomic(&root.join("runtime-maintenance.json"), &result)?;
    Ok(result)
}
async fn maintain(shared: &Arc<Shared>) -> bool {
    let _lock = match maintenance_lock(&shared.root) {
        Ok(Some(lock)) => lock,
        Ok(None) => return false,
        Err(error) => {
            eprintln!(
                "Runtime maintenance could not acquire its lock: {}",
                crate::runtime_diagnostics::bounded_error(error)
            );
            return true;
        }
    };
    let result: Result<()> = async {
        // Cache cleanup failures must not block container or evidence cleanup.
        // Expiration still runs when the service is unavailable.
        let cache = prune_runtime_cache(&shared.root).await;
        let response = match shared
            .rpc("maintenance", &json!({}), shared.stop.clone())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let cache_error = cache
                    .err()
                    .map(|error| format!("; package cache cleanup also failed: {error:#}"))
                    .unwrap_or_default();
                bail!("Service maintenance request failed: {error:#}{cache_error}");
            }
        };
        if let Some(jobs) = response["jobs"].as_array() {
            let mut state = shared.state.lock().await;
            for job in jobs {
                if matches!(
                    job["state"].as_str(),
                    Some("completed" | "superseded" | "cancelled")
                ) && let Some(id) = job["id"].as_str()
                {
                    state.sessions.remove(id);
                }
            }
            let retained: Vec<Value> = jobs
                .iter()
                .map(|job| {
                    let mut job = job.clone();
                    if job["id"]
                        .as_str()
                        .is_some_and(|id| state.active.contains_key(id))
                    {
                        job["state"] = json!("reviewing");
                    }
                    job
                })
                .collect();
            drop(state);
            let outcome = cleanup_runtime_data_locked(
                &shared.root,
                &shared.config["worker"]["execution"],
                &retained,
                &response["retentionDays"],
                cache,
            )
            .await?;
            for warning in outcome["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                eprintln!("Runtime maintenance: {warning}");
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result
        && !shared.stop.is_cancelled()
    {
        let diagnostic = json!({"updatedAt":util::now(),"warningCount":1,"warnings":[crate::runtime_diagnostics::bounded_error(format!("Maintenance could not finish: {error:#}"))]});
        let _ = util::atomic(&shared.root.join("runtime-maintenance.json"), &diagnostic);
        eprintln!(
            "Runtime maintenance deferred; inspect runtime-maintenance.json in Crow's data directory."
        );
    }
    true
}

pub async fn start_worker(config: Value, root: PathBuf) -> Result<WorkerHandle> {
    start_worker_with(
        config,
        root,
        Arc::new(NativeBackend {
            client: reqwest::Client::new(),
        }),
        WorkerOptions::default(),
    )
    .await
}
pub async fn start_worker_with(
    mut config: Value,
    root: PathBuf,
    backend: Arc<dyn WorkerBackend>,
    options: WorkerOptions,
) -> Result<WorkerHandle> {
    crate::config::normalize_numbers(&mut config);
    ensure!(
        config["worker"]["concurrency"].as_u64().unwrap_or(0) > 0,
        "Worker concurrency must be positive"
    );
    let sessions = saved_sessions(&root)?;
    fs::create_dir_all(root.join("logs"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("logs"), fs::Permissions::from_mode(0o700))?;
    }
    let shared = Arc::new(Shared {
        config,
        root,
        backend,
        options,
        state: Mutex::new(State {
            active: BTreeMap::new(),
            sessions,
            claiming: false,
            draining: false,
            closed: false,
            connection: "connecting",
        }),
        changed: Notify::new(),
        wake: Notify::new(),
        maintenance_requested: Notify::new(),
        stop: CancellationToken::new(),
    });
    shared.status().await;
    let running = tokio::spawn(run_loop(shared.clone()));
    let maintenance = {
        let s = shared.clone();
        tokio::spawn(async move {
            loop {
                let delay = if maintain(&s).await {
                    s.options.maintenance
                } else {
                    Duration::from_secs(1)
                };
                tokio::select! {biased; _=s.stop.cancelled()=>break,_=s.maintenance_requested.notified()=>(),_=tokio::time::sleep(delay)=>()}
            }
        })
    };
    let status = {
        let s = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {_=s.stop.cancelled()=>break,_=tokio::time::sleep(s.options.status)=>s.status().await}
            }
        })
    };
    Ok(WorkerHandle {
        shared,
        tasks: Mutex::new(vec![running, maintenance, status]),
    })
}
impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shared.stop.cancel();
    }
}
impl WorkerHandle {
    async fn idle(&self) {
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let s = self.shared.state.lock().await;
                if !s.claiming && s.active.is_empty() {
                    return;
                }
            }
            notified.await;
        }
    }
    pub async fn drain(&self) -> Result<()> {
        self.shared.state.lock().await.draining = true;
        self.shared.status().await;
        self.idle().await;
        Ok(())
    }
    /// Resume claims after an operator or update drain without interrupting running reviews.
    pub async fn undrain(&self) -> Result<()> {
        {
            let mut state = self.shared.state.lock().await;
            ensure!(!state.closed, "Worker stopped");
            state.draining = false;
        }
        self.shared.status().await;
        // notify_one retains a permit if the claim loop has not begun waiting yet.
        self.shared.wake.notify_one();
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        {
            let mut s = self.shared.state.lock().await;
            s.closed = true;
            for active in s.active.values() {
                active.cancel.cancel();
            }
        }
        self.shared.stop.cancel();
        // Serialize concurrent close calls and await provider cleanup, including a pending claim.
        let mut tasks = self.tasks.lock().await;
        for task in tasks.drain(..) {
            task.await?;
        }
        self.idle().await;
        self.shared.status().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::Semaphore;
    const SESSION: &str = "12345678-1234-1234-1234-123456789abc";
    fn report() -> Value {
        json!({"summary":"Inspected the change. No actionable findings.","findings":[]})
    }
    fn work() -> Value {
        json!({"job":{"id":"job1","key":"owner/project#1","repo":"owner/project","number":1,"head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","target":"main","worker":"worker1","state":"reviewing","manual":false,"restart":false,"priority":0,"resumeEpoch":0,"createdAt":1,"updatedAt":1,"retries":0,"nextAt":0,"lease":"lease1","session":null},"pr":{"number":1,"state":"open","draft":false,"user":{"login":"owner"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","ref":"main"},"title":"A change","body":null},"token":"installation-secret"})
    }
    struct Fake {
        queue: Mutex<VecDeque<Value>>,
        events: Mutex<Vec<(String, Value)>>,
        reviews: AtomicUsize,
        fail_report: AtomicBool,
        report_response: Option<Value>,
        fail_session: AtomicBool,
        fail_heartbeat: bool,
        fail_maintenance: bool,
        maintenance_gate: Option<Arc<Semaphore>>,
        execution_enabled: bool,
        retry: bool,
        wait_cancel: bool,
        cancel_session: bool,
        claim_gate: Option<Arc<Semaphore>>,
        review_gate: Option<Arc<Semaphore>>,
        cleanup_gate: Option<Arc<Semaphore>>,
        review_started: Semaphore,
        cleaning: Semaphore,
        guidance_targets: Mutex<Vec<Value>>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                queue: Mutex::new(VecDeque::from([work()])),
                events: Mutex::new(vec![]),
                reviews: AtomicUsize::new(0),
                fail_report: AtomicBool::new(false),
                report_response: None,
                fail_session: AtomicBool::new(false),
                fail_heartbeat: false,
                fail_maintenance: false,
                maintenance_gate: None,
                execution_enabled: true,
                retry: false,
                wait_cancel: false,
                cancel_session: false,
                claim_gate: None,
                review_gate: None,
                cleanup_gate: None,
                review_started: Semaphore::new(0),
                cleaning: Semaphore::new(0),
                guidance_targets: Mutex::new(vec![]),
            }
        }
        async fn count(&self, action: &str) -> usize {
            self.events
                .lock()
                .await
                .iter()
                .filter(|(a, _)| a == action)
                .count()
        }
        async fn until(&self, action: &str, n: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.count(action).await < n {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        }
    }
    #[async_trait]
    impl WorkerBackend for Fake {
        async fn request(
            &self,
            _: &Value,
            action: &str,
            body: &Value,
            _: CancellationToken,
        ) -> Result<Value> {
            self.events.lock().await.push((action.into(), body.clone()));
            match action {
                "next" => {
                    if let Some(gate) = &self.claim_gate {
                        gate.acquire().await.unwrap().forget();
                    }
                    Ok(self.queue.lock().await.pop_front().unwrap_or(Value::Null))
                }
                "maintenance" => {
                    if let Some(gate) = &self.maintenance_gate {
                        gate.acquire().await.unwrap().forget();
                    }
                    if self.fail_maintenance {
                        bail!("Service unavailable");
                    }
                    if self
                        .report_response
                        .as_ref()
                        .is_some_and(|response| response["cancel"] == true)
                        && self.count("report").await > 0
                    {
                        return Ok(
                            json!({"jobs":[{"id":"job1","state":"paused","updatedAt":util::now(),"session":SESSION}],"retentionDays":7}),
                        );
                    }
                    Ok(json!({"jobs":[],"retentionDays":7}))
                }
                "heartbeat" if self.fail_heartbeat => bail!("Connection lost"),
                "report" if self.fail_report.swap(false, Ordering::SeqCst) => {
                    bail!("Lost response")
                }
                "report" if self.report_response.is_some() => {
                    Ok(self.report_response.clone().unwrap())
                }
                "session" if self.fail_session.swap(false, Ordering::SeqCst) => {
                    bail!("Lost session acknowledgment")
                }
                "session" if self.cancel_session => Ok(json!({"cancel":true})),
                "failed" if self.retry => {
                    let mut retry = work();
                    retry["job"]["lease"] = json!("lease2");
                    retry["job"]["session"] = body["session"].clone();
                    self.queue.lock().await.push_back(retry);
                    Ok(json!({"ok":true}))
                }
                _ => Ok(json!({"ok":true})),
            }
        }
        async fn checkout(
            &self,
            root: &Path,
            job: &Value,
            _: &Value,
            _: &str,
            _: CancellationToken,
        ) -> Result<Value> {
            Ok(
                json!({"dir":root,"head":job["head"],"base":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","target":"main","targetSha":"cccccccccccccccccccccccccccccccccccccccc"}),
            )
        }
        async fn guidance(&self, source: &Value) -> Result<Value> {
            self.guidance_targets
                .lock()
                .await
                .push(source["targetSha"].clone());
            Ok(json!({"files":[],"fingerprint":util::hash(&json!([]))}))
        }
        async fn patch(&self, _: &Value, _: &Value, _: CancellationToken) -> Result<String> {
            Ok(String::new())
        }
        async fn review(
            &self,
            _: &Path,
            job: &Value,
            _: &Value,
            _: &Value,
            callbacks: Callbacks,
            cancel: CancellationToken,
        ) -> Result<Value> {
            self.reviews.fetch_add(1, Ordering::SeqCst);
            assert!(job["settings"].get("codexHome").is_some());
            assert!(job["settings"].get("token").is_none());
            assert_eq!(
                crate::execution::enabled(&job["settings"], "owner/project").unwrap(),
                self.execution_enabled,
                "Job-supplied execution settings must not grant authority"
            );
            if let Some(callback) = callbacks.on_session {
                callback(json!(SESSION)).await?;
            }
            self.review_started.add_permits(1);
            if self.wait_cancel {
                cancel.cancelled().await;
                self.cleaning.add_permits(1);
                if let Some(gate) = &self.cleanup_gate {
                    gate.acquire().await.unwrap().forget();
                }
                bail!("Interrupted");
            }
            if let Some(gate) = &self.review_gate {
                gate.acquire().await.unwrap().forget();
            }
            Ok(report())
        }
    }
    fn options() -> WorkerOptions {
        WorkerOptions {
            heartbeat: Duration::from_millis(5),
            poll: Duration::from_millis(3),
            reconnect: Duration::from_millis(3),
            maintenance: Duration::from_secs(3600),
            status: Duration::from_millis(10),
        }
    }
    async fn start(root: &Path, f: Arc<Fake>) -> WorkerHandle {
        let mut config = crate::config::defaults(root);
        config["worker"]["concurrency"] = json!(1);
        start_worker_with(config, root.to_owned(), f, options())
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn maintenance_errors_preserve_receipts_until_resource_cleanup_succeeds() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("podman");
        let id = "a".repeat(32);
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
case "$1" in
 ps) printf '%s\n' '[{{"Names":["crow-experiment-{id}"],"State":"exited"}}]' ;;
 rm) test ! -e "$0.fail" ;;
 *) exit 7 ;;
esac
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.path().join("podman.fail"), "fail").unwrap();
        let experiment = root.path().join("reviews/done/experiments");
        fs::create_dir_all(experiment.join("environments")).unwrap();
        fs::write(experiment.join("environments/prepared.tar"), "dependencies").unwrap();
        util::atomic(
            &experiment.join(format!("{id}.json")),
            &json!({"id":id,"status":"running"}),
        )
        .unwrap();
        let jobs = [json!({"id":"done","state":"completed","updatedAt":0})];
        let config = json!({"podman":executable});
        let first = cleanup_runtime_data(root.path(), &config, &jobs, &json!(0))
            .await
            .unwrap();
        assert_eq!(first["runtime"]["deferredJobs"], json!(["done"]));
        assert!(first["warningCount"].as_u64().unwrap() > 0);
        assert!(experiment.join(format!("{id}.json")).exists());
        fs::remove_file(root.path().join("podman.fail")).unwrap();
        let second = cleanup_runtime_data(root.path(), &config, &jobs, &json!(0))
            .await
            .unwrap();
        assert_eq!(second["warningCount"], 0, "{second}");
        assert_eq!(second["retention"]["removed"], json!(["done"]));
        assert!(!experiment.exists());
        assert_eq!(runtime_maintenance_status(root.path())["warningCount"], 0);
    }
    #[tokio::test]
    async fn reviews_without_runtime_experiments_still_expire() {
        let root = tempfile::tempdir().unwrap();
        let review = root.path().join("reviews/done");
        fs::create_dir_all(&review).unwrap();
        util::atomic(
            &review.join("result.json"),
            &json!({"summary":"Inspection only"}),
        )
        .unwrap();
        let jobs = [json!({"id":"done","state":"completed","updatedAt":0})];
        let result = cleanup_runtime_data(
            root.path(),
            &json!({"podman":"/missing-podman"}),
            &jobs,
            &json!(0),
        )
        .await
        .unwrap();
        assert_eq!(result["warningCount"], 0, "{result}");
        assert_eq!(result["runtime"]["deferredJobs"], json!([]));
        assert_eq!(result["retention"]["removed"], json!(["done"]));
        assert!(!review.exists());
    }
    #[tokio::test]
    async fn confirmed_container_cleanup_allows_retention_without_podman() {
        let root = tempfile::tempdir().unwrap();
        let experiment = root.path().join("reviews/done/experiments");
        fs::create_dir_all(experiment.join("environments")).unwrap();
        fs::write(experiment.join("environments/prepared.tar"), "dependencies").unwrap();
        let id = "a".repeat(32);
        util::atomic(
            &experiment.join(format!("{id}.json")),
            &json!({
                "id":id,"status":"passed","containerStarted":true,"cleanupRecoveredAt":1
            }),
        )
        .unwrap();
        let jobs = [json!({"id":"done","state":"completed","updatedAt":0})];
        let result = cleanup_runtime_data(
            root.path(),
            &json!({"podman":"/missing-podman","enabled":false}),
            &jobs,
            &json!(0),
        )
        .await
        .unwrap();
        assert_eq!(result["warningCount"], 0, "{result}");
        assert_eq!(result["retention"]["removed"], json!(["done"]));
        assert!(!experiment.exists());
    }
    #[tokio::test]
    async fn cache_failure_does_not_prevent_terminal_snapshot_cleanup() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("runtime-cache/packages.lock")).unwrap();
        let experiment = root.path().join("reviews/done/experiments");
        fs::create_dir_all(experiment.join("environments")).unwrap();
        let id = "a".repeat(32);
        util::atomic(
            &experiment.join(format!("{id}.json")),
            &json!({"id":id,"status":"blocked","containerStarted":false}),
        )
        .unwrap();
        let jobs = [json!({"id":"done","state":"completed","updatedAt":util::now()})];
        let result = cleanup_runtime_data(
            root.path(),
            &json!({"podman":"/missing-podman"}),
            &jobs,
            &json!(7),
        )
        .await
        .unwrap();
        assert_eq!(result["warningCount"], 1, "{result}");
        assert!(
            result["warnings"][0]
                .as_str()
                .unwrap()
                .contains("Package cache cleanup")
        );
        assert!(!experiment.join("environments").exists());
        assert!(experiment.join(format!("{id}.json")).exists());
    }
    #[tokio::test]
    async fn concurrent_maintenance_preserves_previous_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let previous = json!({"warningCount":1,"warnings":["Previous cleanup needs retry"]});
        util::atomic(&root.path().join("runtime-maintenance.json"), &previous).unwrap();
        let lock = maintenance_lock(root.path()).unwrap().unwrap();
        let result = cleanup_runtime_data(root.path(), &json!({}), &[], &json!(7))
            .await
            .unwrap();
        assert_eq!(result["skipped"], true);
        assert_eq!(runtime_maintenance_status(root.path()), previous);
        drop(lock);
        let result = cleanup_runtime_data(root.path(), &json!({}), &[], &json!(7))
            .await
            .unwrap();
        assert_eq!(result["warningCount"], 0);
        assert_eq!(runtime_maintenance_status(root.path())["warningCount"], 0);
    }
    #[tokio::test]
    async fn configured_immutable_images_are_protected_without_active_reviews() {
        let root = tempfile::tempdir().unwrap();
        let image = format!("sha256:{}", "a".repeat(64));
        let tag = format!("localhost/crow-runtime:{}", "a".repeat(20));
        let record = root
            .path()
            .join("runtime-cache/images")
            .join(format!("{}.json", crate::runtime::fingerprint(&[&tag])));
        util::atomic(&record, &json!({"tag":tag,"image":image,"lastUsedAt":0})).unwrap();
        // A protected image must be skipped before invoking Podman at all.
        let config =
            json!({"podman":"/missing-podman","repositories":{"owner/project":{"image":image}}});
        let result = cleanup_runtime_data(root.path(), &config, &[], &json!(7))
            .await
            .unwrap();
        assert_eq!(result["warningCount"], 0, "{result}");
        assert_eq!(result["runtime"]["images"], json!([]));
        assert!(record.exists());
    }
    #[test]
    fn corrupt_maintenance_record_is_a_status_warning() {
        let root = tempfile::tempdir().unwrap();
        assert!(runtime_maintenance_status(root.path()).is_null());
        for body in ["{broken", "[]", "null"] {
            fs::write(root.path().join("runtime-maintenance.json"), body).unwrap();
            let result = runtime_maintenance_status(root.path());
            assert_eq!(result["warningCount"], 1);
            assert!(
                result["warnings"][0]
                    .as_str()
                    .unwrap()
                    .contains("runtime-maintenance.json")
            );
        }
    }
    #[tokio::test]
    async fn review_completion_wakes_tracked_maintenance_and_close_waits_for_it() {
        let root = tempfile::tempdir().unwrap();
        let review_gate = Arc::new(Semaphore::new(0));
        let maintenance_gate = Arc::new(Semaphore::new(0));
        let fake = Arc::new(Fake {
            review_gate: Some(review_gate.clone()),
            maintenance_gate: Some(maintenance_gate.clone()),
            ..Fake::new()
        });
        let worker = Arc::new(start(root.path(), fake.clone()).await);
        fake.review_started.acquire().await.unwrap().forget();
        fake.until("maintenance", 1).await;
        maintenance_gate.add_permits(1);
        review_gate.add_permits(1);
        fake.until("report", 1).await;
        fake.until("maintenance", 2).await;
        let closed = {
            let worker = worker.clone();
            tokio::spawn(async move {
                worker.close().await.unwrap();
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!closed.is_finished());
        maintenance_gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), closed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.count("maintenance").await, 2);
    }
    #[tokio::test]
    async fn offline_maintenance_reports_service_and_cache_failures() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("runtime-cache/packages.lock")).unwrap();
        let fake = Arc::new(Fake {
            fail_maintenance: true,
            queue: Mutex::new(VecDeque::new()),
            ..Fake::new()
        });
        let worker = start(root.path(), fake.clone()).await;
        fake.until("maintenance", 1).await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if runtime_maintenance_status(root.path())["warningCount"] == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let value = runtime_maintenance_status(root.path());
        let warning = value["warnings"][0].as_str().unwrap();
        assert!(warning.contains("Service maintenance request failed"));
        assert!(warning.contains("package cache cleanup also failed"));
        worker.close().await.unwrap();
    }
    #[tokio::test]
    async fn worker_reports_live_receipts_and_final_runtime_without_provider_progress() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let f = Arc::new(Fake {
            execution_enabled: true,
            review_gate: Some(gate.clone()),
            ..Fake::new()
        });
        let mut config = crate::config::defaults(dir.path());
        config["worker"]["execution"] = json!({"automatic":true});
        let worker = start_worker_with(config, dir.path().to_owned(), f.clone(), options())
            .await
            .unwrap();
        f.review_started.acquire().await.unwrap().forget();
        let receipt = dir.path().join("reviews/job1/experiments/one.json");
        util::atomic(&receipt, &json!({"phase":"test","status":"running"})).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if f.events.lock().await.iter().any(|(action, body)| {
                    action == "heartbeat" && body["runtime"]["tests"]["running"] == 1
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        util::atomic(&receipt, &json!({"phase":"test","status":"failed"})).unwrap();
        gate.add_permits(1);
        f.until("report", 1).await;
        worker.close().await.unwrap();
        let events = f.events.lock().await;
        let (_, body) = events
            .iter()
            .find(|(action, _)| action == "report")
            .unwrap();
        assert_eq!(body["runtime"]["tests"]["failed"], 1);
        assert_eq!(body["runtime"]["tests"]["running"], 0);
    }

    #[tokio::test]
    async fn service_job_cannot_enable_execution_on_worker() {
        let dir = tempfile::tempdir().unwrap();
        let f = Arc::new(Fake {
            execution_enabled: false,
            ..Fake::new()
        });
        f.queue.lock().await[0]["job"]["settings"] = json!({"execution":{"automatic":true}});
        let mut config = crate::config::defaults(dir.path());
        config["worker"]["execution"] = json!({"automatic":false});
        let worker = start_worker_with(config, dir.path().to_owned(), f.clone(), options())
            .await
            .unwrap();
        f.until("report", 1).await;
        worker.close().await.unwrap();
        assert_eq!(f.reviews.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn only_accepted_reports_release_prepared_snapshots() {
        for acknowledgement in [
            json!({"ok":true}),
            json!({"cancel":true}),
            json!({"ok":true,"cancel":true}),
            json!({"ok":false}),
            json!({}),
        ] {
            let root = tempfile::tempdir().unwrap();
            let experiments = root.path().join("reviews/job1/experiments");
            fs::create_dir_all(experiments.join("environments")).unwrap();
            let snapshot = experiments.join("environments/prepared.tar");
            fs::write(&snapshot, "dependencies needed to resume the paused review").unwrap();
            let id = "a".repeat(32);
            let receipt = experiments.join(format!("{id}.json"));
            util::atomic(
                &receipt,
                &json!({"id":id,"phase":"setup","status":"passed","containerStarted":false}),
            )
            .unwrap();
            let fake = Arc::new(Fake {
                report_response: Some(acknowledgement.clone()),
                ..Fake::new()
            });
            let worker = start(root.path(), fake.clone()).await;
            fake.until("report", 1).await;
            worker.drain().await.unwrap();
            worker.close().await.unwrap();
            let accepted = acknowledgement["ok"] == true && acknowledgement["cancel"] != true;
            assert_eq!(snapshot.exists(), !accepted, "{acknowledgement}");
            assert!(
                receipt.exists(),
                "Runtime evidence must survive {acknowledgement}"
            );
            assert_eq!(
                root.path()
                    .join("reviews/job1/runtime-cleanup.json")
                    .exists(),
                accepted,
                "{acknowledgement}"
            );
            assert_eq!(
                util::read_json(&root.path().join("reports/job1.json"))
                    .unwrap()
                    .unwrap()["report"],
                report()
            );
            if acknowledgement["cancel"] == true {
                assert_eq!(
                    fake.count("failed").await,
                    0,
                    "A rejected lease must not be retried by this execution"
                );
            } else if !accepted {
                assert_eq!(
                    fake.count("failed").await,
                    1,
                    "Missing acknowledgement must be reported for retry"
                );
            }
        }
    }
    #[tokio::test]
    async fn lost_publication_response_reuses_durable_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Fake::new();
        f.fail_report = AtomicBool::new(true);
        f.retry = true;
        let f = Arc::new(f);
        let worker = start(dir.path(), f.clone()).await;
        f.until("report", 2).await;
        worker.drain().await.unwrap();
        worker.close().await.unwrap();
        assert_eq!(f.reviews.load(Ordering::SeqCst), 1);
        assert_eq!(
            util::read_json(&dir.path().join("reports/job1.json"))
                .unwrap()
                .unwrap()["report"],
            report()
        );
        let events = f.events.lock().await;
        let defaults = &events.iter().find(|(a, _)| a == "next").unwrap().1["defaults"];
        for k in ["token", "codex", "codexHome", "id"] {
            assert!(defaults.get(k).is_none());
        }
    }
    #[tokio::test]
    async fn lost_session_ack_is_reconciled_before_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Fake::new();
        f.fail_session = AtomicBool::new(true);
        f.retry = true;
        let f = Arc::new(f);
        let worker = start(dir.path(), f.clone()).await;
        f.until("report", 1).await;
        worker.close().await.unwrap();
        assert_eq!(f.reviews.load(Ordering::SeqCst), 2);
        let events = f.events.lock().await;
        assert_eq!(
            events.iter().find(|(a, _)| a == "failed").unwrap().1["session"],
            SESSION
        );
        assert!(
            events
                .iter()
                .filter(|(a, _)| a == "next")
                .any(|(_, v)| v["sessions"]["job1"] == SESSION)
        );
    }
    #[tokio::test]
    async fn drain_includes_pending_claim_and_waits_for_completion() {
        let dir = tempfile::tempdir().unwrap();
        let claim = Arc::new(Semaphore::new(0));
        let finish = Arc::new(Semaphore::new(0));
        let mut f = Fake::new();
        f.claim_gate = Some(claim.clone());
        f.review_gate = Some(finish.clone());
        let f = Arc::new(f);
        let worker = Arc::new(start(dir.path(), f.clone()).await);
        f.until("next", 1).await;
        let drained = {
            let w = worker.clone();
            tokio::spawn(async move {
                w.drain().await.unwrap();
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        claim.add_permits(1);
        f.review_started.acquire().await.unwrap().forget();
        assert!(!drained.is_finished());
        finish.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), drained)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f.count("next").await, 1);
        worker.close().await.unwrap();
    }
    #[tokio::test]
    async fn undrain_resumes_claims_without_cancelling_existing_review() {
        let dir = tempfile::tempdir().unwrap();
        let finish = Arc::new(Semaphore::new(0));
        let mut fake = Fake::new();
        fake.review_gate = Some(finish.clone());
        let fake = Arc::new(fake);
        let mut config = crate::config::defaults(dir.path());
        config["worker"]["concurrency"] = json!(2);
        let mut timing = options();
        timing.poll = Duration::from_secs(60);
        let worker = Arc::new(
            start_worker_with(config, dir.path().to_owned(), fake.clone(), timing)
                .await
                .unwrap(),
        );
        fake.review_started.acquire().await.unwrap().forget();
        fake.until("next", 2).await;
        let draining = {
            let worker = worker.clone();
            tokio::spawn(async move {
                worker.drain().await.unwrap();
            })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !worker.shared.state.lock().await.draining {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut second = work();
        second["job"]["id"] = json!("job2");
        fake.queue.lock().await.push_back(second);
        assert!(!draining.is_finished());
        draining.abort();
        let _ = draining.await;
        worker.undrain().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), fake.review_started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        {
            let state = worker.shared.state.lock().await;
            assert!(!state.draining);
            assert_eq!(state.active.len(), 2);
            assert!(!state.active["job1"].cancel.is_cancelled());
        }
        assert_eq!(fake.count("failed").await, 0);
        finish.add_permits(2);
        fake.until("report", 2).await;
        worker.close().await.unwrap();
        assert!(worker.undrain().await.is_err());
        assert!(worker.shared.state.lock().await.closed);
    }
    #[tokio::test]
    async fn close_awaits_provider_cleanup_and_preserves_session() {
        let dir = tempfile::tempdir().unwrap();
        let release = Arc::new(Semaphore::new(0));
        let mut f = Fake::new();
        f.wait_cancel = true;
        f.cleanup_gate = Some(release.clone());
        let f = Arc::new(f);
        let worker = Arc::new(start(dir.path(), f.clone()).await);
        f.review_started.acquire().await.unwrap().forget();
        let stopped = {
            let w = worker.clone();
            tokio::spawn(async move {
                w.close().await.unwrap();
            })
        };
        f.cleaning.acquire().await.unwrap().forget();
        assert!(!stopped.is_finished());
        release.add_permits(1);
        stopped.await.unwrap();
        assert_eq!(f.count("report").await, 0);
        let events = f.events.lock().await;
        let failure = &events.iter().find(|(a, _)| a == "failed").unwrap().1;
        assert_eq!(failure["session"], SESSION);
        assert_eq!(failure["kind"], "interrupted");
        let status = util::read_json(&dir.path().join("worker-status.json"))
            .unwrap()
            .unwrap();
        assert_eq!(status["state"], "stopped");
        assert_eq!(status["active"], json!([]));
    }
    #[tokio::test]
    async fn completed_output_racing_cancellation_is_saved_but_never_published() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Fake::new();
        f.cancel_session = true;
        let f = Arc::new(f);
        let worker = start(dir.path(), f.clone()).await;
        f.until("failed", 1).await;
        worker.close().await.unwrap();
        assert_eq!(f.count("report").await, 0);
        assert!(dir.path().join("reports/job1.json").is_file());
    }
    #[tokio::test]
    async fn guidance_is_reconstructed_at_original_target_and_tampering_requires_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = work();
        original["job"]["guidanceFingerprint"] = json!(util::hash(&json!([])));
        original["job"]["guidanceTargetSha"] = json!("original-target");
        let f = Arc::new(Fake::new());
        *f.queue.lock().await = VecDeque::from([original.clone()]);
        let worker = start(dir.path(), f.clone()).await;
        f.until("report", 1).await;
        worker.close().await.unwrap();
        assert_eq!(
            *f.guidance_targets.lock().await,
            vec![json!("original-target")]
        );
        let file = dir.path().join("reviews/job1/guidance.json");
        let mut saved = util::read_json(&file).unwrap().unwrap();
        saved["files"] = json!([{"path":"AGENTS.md","body":"changed"}]);
        util::atomic(&file, &saved).unwrap();
        let f = Arc::new(Fake::new());
        *f.queue.lock().await = VecDeque::from([original]);
        let worker = start(dir.path(), f.clone()).await;
        f.until("failed", 1).await;
        worker.close().await.unwrap();
        assert_eq!(f.reviews.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.events
                .lock()
                .await
                .iter()
                .find(|(a, _)| a == "failed")
                .unwrap()
                .1["kind"],
            "restart"
        );
    }
    #[tokio::test]
    async fn heartbeat_transport_failure_cancels_inference() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Fake::new();
        fake.wait_cancel = true;
        fake.fail_heartbeat = true;
        let fake = Arc::new(fake);
        let worker = start(dir.path(), fake.clone()).await;
        fake.until("failed", 1).await;
        worker.close().await.unwrap();
        assert_eq!(fake.count("report").await, 0);
        assert_eq!(fake.reviews.load(Ordering::SeqCst), 1);
        let events = fake.events.lock().await;
        assert_eq!(
            events.iter().find(|(a, _)| a == "failed").unwrap().1["kind"],
            "interrupted"
        );
    }
    #[tokio::test]
    async fn logs_remove_configured_tokens_encoded_auth_and_common_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::new());
        fake.queue.lock().await.clear();
        let mut config = crate::config::defaults(dir.path());
        config["worker"]["token"] = json!("arbitrary-worker-token");
        config["adminToken"] = json!("arbitrary-admin-token");
        let worker = start_worker_with(config, dir.path().to_owned(), fake, options())
            .await
            .unwrap();
        let token = "installation-secret";
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        let secrets = [
            token,
            "arbitrary-worker-token",
            "arbitrary-admin-token",
            &encoded,
            "ghp_exampletoken",
            "sk-another-secret",
            "other-authorization-token",
        ];
        let message = format!(
            "{} Authorization: Bearer other-authorization-token",
            secrets.join(" ")
        );
        worker.shared.log(&work()["job"], &message, token).unwrap();
        worker.close().await.unwrap();
        let log = fs::read_to_string(dir.path().join("logs/job1.log")).unwrap();
        for secret in &secrets[..6] {
            assert!(!log.contains(secret), "leaked {secret}");
        }
        assert!(log.contains("Authorization: Bearer [redacted]"));
    }
    #[tokio::test]
    async fn service_saved_report_avoids_inference() {
        let dir = tempfile::tempdir().unwrap();
        let f = Arc::new(Fake::new());
        let mut claim = work();
        claim["job"]["report"] = report();
        *f.queue.lock().await = VecDeque::from([claim]);
        let worker = start(dir.path(), f.clone()).await;
        f.until("report", 1).await;
        worker.close().await.unwrap();
        assert_eq!(f.reviews.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn saved_sessions_ignore_links_invalid_ids_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        util::atomic(
            &root.join("reviews/good/session.json"),
            &json!({"id":SESSION}),
        )
        .unwrap();
        util::atomic(
            &root.join("reviews/bad/session.json"),
            &json!({"id":"not-uuid"}),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(root.join("reviews/good"), root.join("reviews/linked")).unwrap();
            fs::create_dir(root.join("reviews/linked-file")).unwrap();
            symlink(
                root.join("reviews/good/session.json"),
                root.join("reviews/linked-file/session.json"),
            )
            .unwrap();
        }
        assert_eq!(
            saved_sessions(root).unwrap(),
            BTreeMap::from([("good".into(), SESSION.into())])
        );
    }
    #[tokio::test]
    async fn http_protocol_rejects_malformed_claims_and_uses_bearer_auth() {
        use axum::{Json, Router, http::HeaderMap, routing::post};
        let payload = Arc::new(Mutex::new(work()));
        let route_payload = payload.clone();
        let app = Router::new().route(
            "/worker/{action}",
            post(move |headers: HeaderMap| {
                let payload = route_payload.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer test-token");
                    Json(payload.lock().await.clone())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let mut c = crate::config::defaults(dir.path());
        c["serviceUrl"] = json!(format!("http://{addr}"));
        c["worker"]["token"] = json!("test-token");
        assert_eq!(
            worker_request(&c, "next", &json!({})).await.unwrap(),
            work()
        );
        let mut cases = vec![];
        for (path, value) in [
            ("/token", json!(42)),
            ("/job/number", json!("1")),
            ("/job/state", json!("invented")),
            ("/pr/head", Value::Null),
            ("/job/id", json!("../escape")),
            ("/job/report", json!({"summary":"incomplete"})),
        ] {
            let mut v = work();
            v["job"]["report"] = Value::Null;
            *v.pointer_mut(path)
                .unwrap_or_else(|| panic!("missing {path}")) = value;
            cases.push(("next", v));
        }
        cases.extend([
            ("maintenance", json!({"jobs":{},"retentionDays":7})),
            ("heartbeat", json!({"cancel":"false"})),
            ("comparison", json!({"skip":1})),
            ("ping", json!({"ok":true,"id":null})),
        ]);
        for (action, value) in cases {
            *payload.lock().await = value;
            assert!(
                worker_request(&c, action, &json!({})).await.is_err(),
                "accepted malformed {action}"
            );
        }
        *payload.lock().await = Value::Null;
        assert!(
            worker_request(&c, "next", &json!({}))
                .await
                .unwrap()
                .is_null()
        );
        task.abort();
    }
}
