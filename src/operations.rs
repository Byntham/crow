//! User-service lifecycle, diagnostics, and transactional native updates.
use crate::github::GitHubApi;
use crate::{install, util};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep};

pub async fn run(command: &str, args: &[&str], inherit: bool) -> Result<String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let output = crate::process::run(
        command,
        &args,
        crate::process::RunOptions {
            env: Some(util::host_env().into_iter().collect()),
            timeout: if inherit {
                None
            } else {
                Some(Duration::from_secs(120))
            },
            detached: !inherit,
            inherit,
            ..Default::default()
        },
    )
    .await?;
    Ok(output.stdout)
}

fn absolute(root: &Path) -> PathBuf {
    let path = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(root)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub fn unit_name(root: &Path) -> String {
    let digest = util::hash_bytes(absolute(root).to_string_lossy().as_bytes());
    format!("crow-{}.service", &digest[..16])
}

fn unit_quote(value: &str, expands_dollars: bool) -> String {
    let value = if expands_dollars {
        value.replace('$', "$$")
    } else {
        value.to_owned()
    };
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

pub fn unit_text(root: &Path, executable: &Path, path: &str) -> String {
    let executable = absolute(executable);
    let command = unit_quote(&executable.to_string_lossy(), true);
    let home = unit_quote(&format!("CROW_HOME={}", absolute(root).display()), false);
    let env_path = unit_quote(
        &format!(
            "PATH={}:{}",
            executable.parent().unwrap_or(Path::new("/")).display(),
            path
        ),
        false,
    );
    format!(
        "[Unit]\nDescription=Crow PR review service\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={command} run\nEnvironment={home}\nEnvironment={env_path}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=45\nUMask=0077\n\n[Install]\nWantedBy=default.target\n"
    )
}

pub async fn install_service(root: &Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!(
            "Persistent startup requires Linux and systemd. Use crow run in your service manager."
        );
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let base = PathBuf::from(home).join(".config/systemd/user");
    std::fs::create_dir_all(&base)?;
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    util::atomic_bytes(
        &base.join(unit_name(root)),
        unit_text(root, &install::binary_path(root), &path).as_bytes(),
    )?;
    run("systemctl", &["--user", "daemon-reload"], false).await?;
    let user = run("id", &["-un"], false).await?.trim().to_owned();
    let linger = run(
        "loginctl",
        &["show-user", &user, "--property=Linger", "--value"],
        false,
    )
    .await?;
    if linger.trim() != "yes"
        && run("loginctl", &["enable-linger", &user], true)
            .await
            .is_err()
    {
        run("sudo", &["loginctl", "enable-linger", &user], true).await?;
    }
    run(
        "systemctl",
        &["--user", "enable", "--now", &unit_name(root)],
        true,
    )
    .await?;
    Ok(())
}

pub async fn service_action(root: &Path, action: &str) -> Result<()> {
    if !["start", "stop", "restart", "status"].contains(&action) {
        bail!("Invalid service action");
    }
    run("systemctl", &["--user", action, &unit_name(root)], true).await?;
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub async fn admin(config: &Value, action: &str, args: &Value) -> Result<Value> {
    if config["role"] == "worker" {
        bail!("Run this command on the connection-service host.");
    }
    if action.is_empty() || !action.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        bail!("Invalid administration action");
    }
    let fallback = format!(
        "http://127.0.0.1:{}",
        config["port"].as_u64().unwrap_or(8787)
    );
    let base = config["serviceUrl"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&fallback);
    let url = url::Url::parse(base)?.join(&format!("/admin/{action}"))?;
    let client = client()?;
    let mut request = if args.is_null() {
        client.get(url)
    } else {
        client.post(url).json(args)
    };
    request = request.bearer_auth(
        config["adminToken"]
            .as_str()
            .context("Missing administration token")?,
    );
    let response = request.send().await.with_context(|| {
        format!(
            "Could not reach Crow at {base}. Run crow start on the service machine, then crow doctor."
        )
    })?;
    let status = response.status();
    let body: Value = response.json().await.context("Invalid Crow response")?;
    if !status.is_success() {
        bail!(
            "{}",
            body["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Crow returned HTTP {status}"))
        );
    }
    if action == "status"
        && (!body["jobs"]
            .as_array()
            .is_some_and(|jobs| jobs.iter().all(|j| j["state"].is_string()))
            || !body["repos"]
                .as_array()
                .is_some_and(|repos| repos.iter().all(|r| r["name"].is_string())))
    {
        bail!("Invalid Crow status response");
    }
    Ok(body)
}

pub async fn wait_for_service(config: &Value) -> Result<Value> {
    let mut error = None;
    for attempt in 0..30 {
        match admin(config, "status", &Value::Null).await {
            Ok(status) => return Ok(status),
            Err(e) => error = Some(e),
        }
        if attempt < 29 {
            sleep(Duration::from_secs(1)).await;
        }
    }
    Err(error.context("Service startup failed")?)
}

fn process_id(file: &Path) -> Result<Option<i32>> {
    let value = util::read_json(file)?;
    match value.as_ref().and_then(|v| v.get("pid")) {
        None => Ok(None),
        Some(pid) => {
            let pid = pid
                .as_i64()
                .filter(|pid| *pid > 0 && *pid <= i32::MAX as i64)
                .context("Invalid Crow process identifier")?;
            if let Some(expected) = value.as_ref().and_then(|v| v["start"].as_str())
                && let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            {
                let actual = stat
                    .rfind(')')
                    .and_then(|end| stat.get(end + 2..))
                    .and_then(|fields| fields.split_whitespace().nth(19));
                if actual.is_some_and(|actual| actual != expected) {
                    return Ok(None);
                }
            }
            Ok(Some(pid as i32))
        }
    }
}
fn alive(pid: i32) -> bool {
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// A saved status file alone does not establish that its worker still runs.
pub fn worker_running(root: &Path) -> Result<bool> {
    let Some(pid) = process_id(&root.join("runtime.lock"))? else {
        return Ok(false);
    };
    let status = util::read_json(&root.join("worker-status.json"))?.unwrap_or(Value::Null);
    Ok(alive(pid) && status["pid"].as_i64() == Some(i64::from(pid)))
}

pub async fn wait_for_startup(root: &Path, expected_version: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let main = run(
            "systemctl",
            &[
                "--user",
                "show",
                &unit_name(root),
                "--property=MainPID",
                "--value",
            ],
            false,
        )
        .await?;
        let pid = main.trim().parse::<i32>().unwrap_or(0);
        if let Some(ready) = util::read_json(&root.join("ready.json"))?
            && pid > 0
            && ready["pid"].as_i64() == Some(i64::from(pid))
            && ready["version"] == expected_version
            && process_id(&root.join("runtime.lock"))? == Some(pid)
            && alive(pid)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Crow did not finish starting");
        }
        sleep(Duration::from_millis(500)).await;
    }
}

pub async fn clear_service_drain(root: &Path, config: &Value) -> Result<()> {
    if admin(config, "undrain", &json!({})).await.is_ok() {
        return Ok(());
    }
    let database = rusqlite::Connection::open_with_flags(
        root.join("service.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    database.busy_timeout(Duration::from_secs(5))?;
    database.execute(
        "DELETE FROM records WHERE kind = 'state' AND id = 'drain'",
        [],
    )?;
    Ok(())
}

async fn wait_for_reviews(config: &Value) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(86400);
    loop {
        let status = admin(config, "status", &Value::Null).await?;
        if !status["jobs"]
            .as_array()
            .context("Invalid jobs")?
            .iter()
            .any(|job| job["state"] == "reviewing")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Timed out waiting for active reviews; Crow has not been stopped");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

pub async fn drain_worker(root: &Path) -> Result<()> {
    let Some(pid) = process_id(&root.join("runtime.lock"))? else {
        return Ok(());
    };
    if !alive(pid) {
        return Ok(());
    }
    match std::fs::remove_file(root.join("drained.json")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let deadline = Instant::now() + Duration::from_secs(86400);
    loop {
        if process_id(&root.join("drained.json"))? == Some(pid) {
            return Ok(());
        }
        if !alive(pid) {
            bail!("Worker exited while draining");
        }
        if Instant::now() >= deadline {
            bail!("Timed out waiting for worker drain; Crow has not been stopped");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Snapshot ownership before requesting a drain. Never resume a different process.
#[derive(Clone, Debug)]
pub struct WorkerDrainState {
    pub pid: i32,
    pub already_draining: bool,
    start: Option<String>,
}
fn native_drain_control(root: &Path, pid: i32) -> Result<bool> {
    let ready = util::read_json(&root.join("ready.json"))?.unwrap_or(Value::Null);
    let parts = ready["version"].as_str().and_then(|v| {
        v.split('.')
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()
    });
    Ok(ready["pid"].as_i64() == Some(i64::from(pid))
        && parts.is_some_and(|p| p.len() == 3 && p.as_slice() >= [0, 3, 0].as_slice()))
}
pub fn worker_drain_state(root: &Path) -> Result<Option<WorkerDrainState>> {
    let Some(pid) = process_id(&root.join("runtime.lock"))? else {
        return Ok(None);
    };
    if !alive(pid) {
        return Ok(None);
    }
    if !native_drain_control(root, pid)? {
        bail!(
            "This running worker does not support resumable native drains. Use its installed Crow update command."
        );
    }
    let status = util::read_json(&root.join("worker-status.json"))?
        .context("Worker status is missing; cannot establish drain ownership")?;
    if status["pid"].as_i64() != Some(i64::from(pid))
        || !matches!(status["state"].as_str(), Some("running" | "draining"))
    {
        bail!("Worker status does not identify the running worker");
    }
    let lock =
        util::read_json(&root.join("runtime.lock"))?.context("Worker runtime disappeared")?;
    Ok(Some(WorkerDrainState {
        pid,
        already_draining: status["state"] == "draining",
        start: lock["start"].as_str().map(str::to_owned),
    }))
}
pub async fn resume_worker(root: &Path, previous: &WorkerDrainState) -> Result<()> {
    if previous.already_draining {
        return Ok(());
    }
    let matches = || -> Result<bool> {
        if process_id(&root.join("runtime.lock"))? != Some(previous.pid) || !alive(previous.pid) {
            return Ok(false);
        }
        let lock = util::read_json(&root.join("runtime.lock"))?.unwrap_or(Value::Null);
        Ok(lock["start"].as_str() == previous.start.as_deref())
    };
    if !matches()? {
        return Ok(());
    }
    if !native_drain_control(root, previous.pid)? {
        bail!("Refusing to signal resume to a runtime without native drain control");
    }
    match std::fs::remove_file(root.join("undrained.json")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if unsafe { libc::kill(previous.pid, libc::SIGUSR2) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error.into());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !matches()? {
            return Ok(());
        }
        let status = util::read_json(&root.join("worker-status.json"))?.unwrap_or(Value::Null);
        let acknowledged = process_id(&root.join("undrained.json"))? == Some(previous.pid);
        if acknowledged
            && status["pid"].as_i64() == Some(i64::from(previous.pid))
            && status["state"] == "running"
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Worker did not acknowledge resume after interrupted update");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Listen for both ordinary terminal interruption and service-manager termination.
pub async fn interrupted() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateAction {
    WorkerStatus,
    DrainWorker,
    ResumeWorker,
    ServiceStatus,
    DrainService,
    WaitReviews,
    Stop,
    Activate,
    Start,
    Ready,
    Rollback,
    ClearServiceDrain,
}
trait UpdateLifecycle {
    async fn action(&mut self, action: UpdateAction) -> Result<Value>;
}
struct NativeUpdate<'a> {
    config: &'a Value,
    root: &'a Path,
    release: install::PreparedUpdate,
    activation: Option<install::Activation>,
    worker: Option<WorkerDrainState>,
}
impl UpdateLifecycle for NativeUpdate<'_> {
    async fn action(&mut self, action: UpdateAction) -> Result<Value> {
        match action {
            UpdateAction::WorkerStatus => {
                self.worker = worker_drain_state(self.root)?;
                return Ok(
                    json!({"running":self.worker.is_some(),"draining":self.worker.as_ref().is_some_and(|w|w.already_draining)}),
                );
            }
            UpdateAction::DrainWorker => drain_worker(self.root).await?,
            UpdateAction::ResumeWorker => {
                if let Some(worker) = &self.worker {
                    resume_worker(self.root, worker).await?;
                }
            }
            UpdateAction::ServiceStatus => return admin(self.config, "status", &Value::Null).await,
            UpdateAction::DrainService => {
                admin(self.config, "drain", &json!({})).await?;
            }
            UpdateAction::WaitReviews => wait_for_reviews(self.config).await?,
            UpdateAction::Stop => service_action(self.root, "stop").await?,
            UpdateAction::Activate => {
                match std::fs::remove_file(self.root.join("ready.json")) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                self.activation = Some(self.release.activate()?);
            }
            UpdateAction::Start => service_action(self.root, "start").await?,
            UpdateAction::Ready => {
                wait_for_startup(self.root, &self.release.version).await?;
                if self.config["role"] != "worker" {
                    wait_for_service(self.config).await?;
                }
            }
            UpdateAction::Rollback => self
                .activation
                .take()
                .context("No activated release to restore")?
                .rollback()?,
            UpdateAction::ClearServiceDrain => clear_service_drain(self.root, self.config).await?,
        }
        Ok(Value::Null)
    }
}

async fn apply_update<L: UpdateLifecycle, F: std::future::Future<Output = Result<()>>>(
    lifecycle: &mut L,
    worker: bool,
    interruption: F,
) -> Result<()> {
    let mut owns_service_drain = false;
    let mut owns_worker_drain = false;
    let mut stop_requested = false;
    let mut activated = false;
    let operation = async {
        if worker {
            let state = lifecycle.action(UpdateAction::WorkerStatus).await?;
            owns_worker_drain = state["running"] == true && state["draining"] != true;
            lifecycle.action(UpdateAction::DrainWorker).await?;
        } else {
            let state = lifecycle.action(UpdateAction::ServiceStatus).await?;
            if state["draining"] != true {
                owns_service_drain = true;
                lifecycle.action(UpdateAction::DrainService).await?;
            }
            lifecycle.action(UpdateAction::WaitReviews).await?;
        }
        // systemd may continue the stop after its client exits or the reply is lost.
        stop_requested = true;
        lifecycle.action(UpdateAction::Stop).await?;
        lifecycle.action(UpdateAction::Activate).await?;
        activated = true;
        lifecycle.action(UpdateAction::Start).await?;
        lifecycle.action(UpdateAction::Ready).await?;
        Ok(())
    };
    let result: Result<()> = tokio::select! {
        result = operation => result,
        signal = interruption => match signal { Ok(()) => Err(anyhow::anyhow!("Update interrupted")), Err(error) => Err(error.context("Cannot listen for update interruption")) }
    };
    let mut errors = Vec::new();
    if let Err(error) = &result {
        errors.push(format!("{error:#}"));
        if activated {
            if let Err(stop) = lifecycle.action(UpdateAction::Stop).await {
                errors.push(format!("Could not stop failed release: {stop:#}"));
            }
            if let Err(rollback) = lifecycle.action(UpdateAction::Rollback).await {
                errors.push(format!(
                    "Could not restore previous executable: {rollback:#}"
                ));
            }
        }
        if stop_requested {
            // Start also cancels a systemd stop job whose failed client left it pending.
            if let Err(start) = lifecycle.action(UpdateAction::Start).await {
                errors.push(format!("Could not restart selected runtime: {start:#}"));
            }
        }
        // An interrupted drain has not stopped the worker or its active reviews.
        if owns_worker_drain && let Err(resume) = lifecycle.action(UpdateAction::ResumeWorker).await
        {
            errors.push(format!("Could not resume worker: {resume:#}"));
        }
    }
    if owns_service_drain
        && let Err(clear) = lifecycle.action(UpdateAction::ClearServiceDrain).await
    {
        errors.push(format!(
            "Could not clear update drain: {clear:#}. Run crow undrain after restoring the service"
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

pub async fn update(config: &Value, root: &Path) -> Result<Value> {
    let _lock = install::acquire_install_lock(root)?;
    let Some(release) = install::prepare_update(root, env!("CARGO_PKG_VERSION")).await? else {
        return Ok(json!({"updated":false,"message":"Crow is current."}));
    };
    let version = release.version.clone();
    let mut lifecycle = NativeUpdate {
        config,
        root,
        release,
        activation: None,
        worker: None,
    };
    apply_update(&mut lifecycle, config["role"] == "worker", interrupted()).await?;
    Ok(json!({"updated":true,"version":version}))
}

pub async fn update_availability(root: &Path) -> Result<Value> {
    let file = root.join("update-status.json");
    let previous = util::read_json(&file).ok().flatten().unwrap_or(Value::Null);
    let now = util::now();
    if previous["checkedAt"]
        .as_i64()
        .is_some_and(|at| at <= now && now - at < 86_400_000)
    {
        let mut result = previous;
        if let Some(map) = result.as_object_mut() {
            map.insert("cached".into(), json!(true));
        }
        return Ok(result);
    }
    let check = install::check_update(env!("CARGO_PKG_VERSION")).await?;
    let warning = check["warning"].as_str();
    let mut result = json!({"checkedAt":now,"available":if warning.is_some() {previous["available"].clone()} else {check["available"].clone()},"version":check.get("version").or_else(||previous.get("version")).cloned().unwrap_or(Value::Null),"cached":warning.is_some() && !previous.is_null()});
    if let Some(warning) = warning {
        result["warning"] = json!(warning);
    }
    let _ = util::atomic(&file, &result);
    Ok(result)
}

async fn health(url: &str) -> Result<Value> {
    let response = client()?
        .get(url::Url::parse(url)?.join("/health")?)
        .send()
        .await?
        .error_for_status()?;
    let body: Value = response.json().await?;
    if body["service"] != "crow" || body["configured"] != true {
        bail!("HTTPS does not reach this configured Crow service");
    }
    Ok(body)
}
fn check_result(checks: &mut Vec<Value>, name: &str, result: Result<Value>) {
    checks.push(match result {
        Ok(detail) => json!({"name":name,"ok":true,"detail":detail}),
        Err(error) => json!({"name":name,"ok":false,"detail":format!("{error:#}")}),
    });
}

fn service_summary(status: Value) -> Value {
    let count = |key: &str| status[key].as_array().map_or(0, Vec::len);
    let active = status["jobs"].as_array().map_or(0, |jobs| {
        jobs.iter()
            .filter(|job| {
                matches!(
                    job["state"].as_str(),
                    Some("queued" | "held" | "reviewing" | "retrying" | "paused" | "publishing")
                )
            })
            .count()
    });
    json!({"repositories":count("repos"),"workers":count("workers"),"jobs":count("jobs"),"activeJobs":active,"draining":status["draining"] == true})
}

pub async fn doctor(config: &Value, root: &Path, runtime: bool) -> Result<Value> {
    let mut checks = Vec::new();
    if config["role"] != "worker" {
        check_result(
            &mut checks,
            "local connection service",
            admin(config, "status", &Value::Null)
                .await
                .map(service_summary),
        );
        check_result(
            &mut checks,
            "public HTTPS",
            health(config["publicUrl"].as_str().unwrap_or("")).await,
        );
        let app = async {
            let app_config = config
                .get("app")
                .filter(|v| v.is_object())
                .context("GitHub App registration incomplete")?;
            let token = crate::github::jwt(app_config)?;
            let github = crate::github::GitHub::new(Some(app_config.clone()));
            let app = github.request("/app", Some(&token), "GET", None).await?;
            crate::setup::validate_app_requirements(&app)?;
            let hook = github
                .request("/app/hook/config", Some(&token), "GET", None)
                .await?;
            crate::setup::validate_app_webhook(
                &hook,
                &format!(
                    "{}/webhooks/github",
                    config["publicUrl"].as_str().unwrap_or("")
                ),
            )?;
            Ok(app["slug"].clone())
        }
        .await;
        check_result(&mut checks, "GitHub App", app);
    } else {
        check_result(
            &mut checks,
            "connection service reachable",
            health(config["serviceUrl"].as_str().unwrap_or("")).await,
        );
    }
    if config["role"] != "service" {
        let pairing = async {
            let url = url::Url::parse(
                config["serviceUrl"]
                    .as_str()
                    .context("Missing service URL")?,
            )?
            .join("/worker/ping")?;
            let response = client()?
                .post(url)
                .bearer_auth(
                    config["worker"]["token"]
                        .as_str()
                        .context("Missing worker token")?,
                )
                .json(&json!({}))
                .send()
                .await?
                .error_for_status()?;
            let body: Value = response.json().await?;
            if body["id"] != config["worker"]["id"] {
                bail!("Pairing belongs to another worker");
            }
            Ok(body["id"].clone())
        }
        .await;
        check_result(&mut checks, "worker pairing", pairing);
        let auth = async {
            let auth = crate::provider::auth_status(&config["worker"], root).await?;
            if auth["authenticated"] != true {
                bail!("{}", auth["warning"].as_str().unwrap_or("Run crow login"));
            }
            Ok(auth["account"].clone())
        }
        .await;
        check_result(&mut checks, "subscription authentication", auth);
        if runtime {
            let diagnostics = async {
                let result = crate::provider::diagnostics(&config["worker"], root, true).await?;
                if result["ok"] != true {
                    bail!("{result}");
                }
                Ok(result)
            }
            .await;
            check_result(&mut checks, "review runtime", diagnostics);
            check_result(
                &mut checks,
                "runtime experiments",
                crate::execution::diagnostics(&config["worker"]).await,
            );
        }
    }
    let persistent = async {
        let enabled = run(
            "systemctl",
            &["--user", "is-enabled", &unit_name(root)],
            false,
        )
        .await?;
        let active = run(
            "systemctl",
            &["--user", "is-active", &unit_name(root)],
            false,
        )
        .await?;
        if active.trim() != "active" {
            bail!("Crow service is not active");
        }
        Ok(json!(format!("{}, {}", enabled.trim(), active.trim())))
    }
    .await;
    check_result(&mut checks, "persistent startup", persistent);
    Ok(json!({"ok":checks.iter().all(|c|c["ok"] == true),"checks":checks}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_worker_status_must_match_a_live_runtime() {
        let root = tempfile::tempdir().unwrap();
        assert!(!worker_running(root.path()).unwrap());
        util::atomic(
            &root.path().join("worker-status.json"),
            &json!({"pid":std::process::id(),"state":"running"}),
        )
        .unwrap();
        assert!(!worker_running(root.path()).unwrap());
        util::atomic(
            &root.path().join("runtime.lock"),
            &json!({"pid":std::process::id()}),
        )
        .unwrap();
        assert!(worker_running(root.path()).unwrap());
        util::atomic(
            &root.path().join("runtime.lock"),
            &json!({"pid":std::process::id(),"start":"old-process"}),
        )
        .unwrap();
        assert!(!worker_running(root.path()).unwrap());
    }
    #[test]
    fn successful_service_diagnostics_summarize_history_and_preserve_errors() {
        let history = json!({
            "repos":[{"name":"owner/private"}],
            "workers":[{"id":"worker-a"},{"id":"worker-b"}],
            "jobs":[{"state":"completed","session":"private-session"},{"state":"reviewing"},{"state":"paused"},{"state":"cancelled"}],
            "draining":true
        });
        let mut checks = Vec::new();
        check_result(
            &mut checks,
            "local connection service",
            Ok(service_summary(history)),
        );
        assert_eq!(
            checks[0]["detail"],
            json!({"repositories":1,"workers":2,"jobs":4,"activeJobs":2,"draining":true})
        );
        assert!(!checks[0].to_string().contains("private"));
        check_result(
            &mut checks,
            "local connection service",
            Err(anyhow::anyhow!("Connection refused")),
        );
        assert_eq!(checks[1]["detail"], "Connection refused");
        assert_eq!(checks[1]["ok"], false);
    }
    #[test]
    fn service_units_quote_directives_and_native_executable() {
        let text = unit_text(
            Path::new("/tmp/a %b$\"c"),
            Path::new("/tmp/a %b$\"c/current/crow"),
            "/usr/bin",
        );
        assert!(text.contains("ExecStart=\"/tmp/a %%b$$\\\"c/current/crow\" run\n"));
        assert!(text.contains("Environment=\"CROW_HOME=/tmp/a %%b$\\\"c\"\n"));
        assert!(text.contains("KillMode=mixed\n"));
        assert!(text.contains("UMask=0077\n"));
        assert!(!text.contains("node"));
        assert_eq!(
            unit_name(Path::new("/a/../b/./c")),
            unit_name(Path::new("/b/c"))
        );
    }
    #[tokio::test]
    async fn admin_rejects_worker_and_invalid_routes_before_network() {
        assert!(
            admin(&json!({"role":"worker"}), "status", &Value::Null)
                .await
                .unwrap_err()
                .to_string()
                .contains("connection-service")
        );
        assert!(
            admin(&json!({"role":"both"}), "../status", &Value::Null)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn admin_preserves_auth_and_rejects_malformed_status() {
        use axum::{
            Json, Router,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        let app = Router::new().route(
            "/admin/status",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
                (
                    StatusCode::OK,
                    Json(json!({"jobs":[{"state":5}],"repos":[]})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config =
            json!({"role":"both","serviceUrl":format!("http://{address}"),"adminToken":"secret"});
        assert!(
            admin(&config, "status", &Value::Null)
                .await
                .unwrap_err()
                .to_string()
                .contains("Invalid Crow status")
        );
        server.abort();
    }
    #[test]
    fn stale_process_identity_is_never_used_for_signals() {
        let root = tempfile::tempdir().unwrap();
        let lock = root.path().join("runtime.lock");
        util::atomic(
            &lock,
            &json!({"pid":std::process::id(),"start":"not-this-process"}),
        )
        .unwrap();
        assert_eq!(process_id(&lock).unwrap(), None);
        util::atomic(&lock, &json!({"pid":-1})).unwrap();
        assert!(process_id(&lock).is_err());
    }
    #[tokio::test]
    async fn failed_http_undrain_removes_only_the_persistent_drain() {
        let root = tempfile::tempdir().unwrap();
        let db = rusqlite::Connection::open(root.path().join("service.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE records(kind TEXT,id TEXT,value TEXT,PRIMARY KEY(kind,id)); INSERT INTO records VALUES('state','drain','true'),('state','other','true'),('jobs','drain','{}');").unwrap();
        let config = json!({"role":"both","serviceUrl":"http://127.0.0.1:0","adminToken":"secret"});
        clear_service_drain(root.path(), &config).await.unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM records", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    struct FakeLifecycle {
        calls: Vec<UpdateAction>,
        fail: Vec<UpdateAction>,
        initial_drain: bool,
        draining: bool,
        active_reviews: usize,
        interrupt_at: Option<UpdateAction>,
        interrupted: std::sync::Arc<tokio::sync::Notify>,
    }
    impl FakeLifecycle {
        fn new(fail: Vec<UpdateAction>) -> Self {
            Self {
                calls: vec![],
                fail,
                initial_drain: false,
                draining: false,
                active_reviews: 0,
                interrupt_at: None,
                interrupted: std::sync::Arc::new(tokio::sync::Notify::new()),
            }
        }
    }
    impl UpdateLifecycle for FakeLifecycle {
        async fn action(&mut self, action: UpdateAction) -> Result<Value> {
            self.calls.push(action);
            if action == UpdateAction::DrainWorker || action == UpdateAction::DrainService {
                self.draining = true;
            }
            if self.interrupt_at == Some(action) {
                self.interrupted.notify_one();
                std::future::pending::<()>().await;
            }
            if let Some(index) = self.fail.iter().position(|failure| *failure == action) {
                self.fail.remove(index);
                bail!("injected {action:?} failure");
            }
            match action {
                UpdateAction::WorkerStatus => {
                    Ok(json!({"running":true,"draining":self.initial_drain}))
                }
                UpdateAction::ServiceStatus => Ok(json!({"draining":self.initial_drain})),
                UpdateAction::ResumeWorker | UpdateAction::ClearServiceDrain => {
                    self.draining = false;
                    Ok(Value::Null)
                }
                _ => Ok(Value::Null),
            }
        }
    }
    async fn uninterrupted_update(fake: &mut FakeLifecycle, worker: bool) -> Result<()> {
        apply_update(fake, worker, std::future::pending()).await
    }
    #[tokio::test]
    async fn interrupted_worker_drain_resumes_without_stopping_active_reviews() {
        let mut fake = FakeLifecycle::new(vec![]);
        fake.active_reviews = 2;
        fake.interrupt_at = Some(UpdateAction::DrainWorker);
        let notification = fake.interrupted.clone();
        let result = apply_update(&mut fake, true, async move {
            notification.notified().await;
            Ok(())
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("interrupted"));
        assert_eq!(fake.active_reviews, 2);
        assert!(!fake.draining);
        assert_eq!(
            fake.calls,
            vec![
                UpdateAction::WorkerStatus,
                UpdateAction::DrainWorker,
                UpdateAction::ResumeWorker
            ]
        );
    }
    #[tokio::test]
    async fn worker_stop_and_activation_failures_restart_selected_runtime_and_resume() {
        for failure in [UpdateAction::Stop, UpdateAction::Activate] {
            let mut fake = FakeLifecycle::new(vec![failure]);
            assert!(uninterrupted_update(&mut fake, true).await.is_err());
            let recovery = fake.calls.iter().rposition(|a| *a == failure).unwrap();
            assert_eq!(
                &fake.calls[recovery + 1..],
                &[UpdateAction::Start, UpdateAction::ResumeWorker]
            );
            assert!(!fake.draining);
        }
    }
    #[tokio::test]
    async fn failed_start_or_readiness_rolls_back_before_restarting() {
        for failure in [UpdateAction::Start, UpdateAction::Ready] {
            let mut fake = FakeLifecycle::new(vec![failure]);
            assert!(uninterrupted_update(&mut fake, false).await.is_err());
            let rollback = fake
                .calls
                .iter()
                .position(|a| *a == UpdateAction::Rollback)
                .unwrap();
            assert_eq!(fake.calls[rollback - 1], UpdateAction::Stop);
            assert_eq!(
                &fake.calls[rollback + 1..],
                &[UpdateAction::Start, UpdateAction::ClearServiceDrain]
            );
            assert!(!fake.draining);
        }
    }
    #[tokio::test]
    async fn update_restores_only_its_own_drain_on_success_and_failure() {
        for worker in [false, true] {
            for existing in [false, true] {
                for failure in [None, Some(UpdateAction::Stop), Some(UpdateAction::Ready)] {
                    let mut fake = FakeLifecycle::new(failure.into_iter().collect());
                    fake.initial_drain = existing;
                    fake.draining = existing;
                    let result = uninterrupted_update(&mut fake, worker).await;
                    assert_eq!(result.is_err(), failure.is_some());
                    let cleanup = if worker {
                        UpdateAction::ResumeWorker
                    } else {
                        UpdateAction::ClearServiceDrain
                    };
                    assert_eq!(
                        fake.calls.contains(&cleanup),
                        !existing && (!worker || failure.is_some())
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn lost_drain_response_and_cleanup_errors_are_not_hidden() {
        let mut fake = FakeLifecycle::new(vec![
            UpdateAction::DrainService,
            UpdateAction::ClearServiceDrain,
        ]);
        let error = uninterrupted_update(&mut fake, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("DrainService"));
        assert!(error.contains("ClearServiceDrain"));
        assert!(!fake.calls.contains(&UpdateAction::Stop));
    }
    #[tokio::test]
    async fn interrupted_stop_recovers_even_before_stop_reply() {
        let mut fake = FakeLifecycle::new(vec![]);
        fake.interrupt_at = Some(UpdateAction::Stop);
        let notification = fake.interrupted.clone();
        assert!(
            apply_update(&mut fake, true, async move {
                notification.notified().await;
                Ok(())
            })
            .await
            .is_err()
        );
        assert!(
            fake.calls
                .ends_with(&[UpdateAction::Start, UpdateAction::ResumeWorker])
        );
    }
    #[test]
    fn legacy_worker_is_rejected_before_an_unsafe_resume_signal() {
        let dir = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        util::atomic(&dir.path().join("runtime.lock"), &json!({"pid":pid})).unwrap();
        util::atomic(
            &dir.path().join("worker-status.json"),
            &json!({"pid":pid,"state":"running"}),
        )
        .unwrap();
        util::atomic(
            &dir.path().join("ready.json"),
            &json!({"pid":pid,"version":"0.2.0"}),
        )
        .unwrap();
        assert!(
            worker_drain_state(dir.path())
                .unwrap_err()
                .to_string()
                .contains("native drains")
        );
        util::atomic(
            &dir.path().join("ready.json"),
            &json!({"pid":pid,"version":"0.3.0"}),
        )
        .unwrap();
        assert!(
            !worker_drain_state(dir.path())
                .unwrap()
                .unwrap()
                .already_draining
        );
    }
}
