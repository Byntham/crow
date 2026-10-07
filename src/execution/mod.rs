//! Opt-in runtime experiments (ADR 0010). The main reviewer can install
//! dependencies and run focused commands in rootless Podman, but only for
//! repositories the worker operator lists and PRs the connection service
//! reports as coming from trusted authors.
mod gateway;
pub mod image;
mod sandbox;
mod seccomp;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// Resource limits for each container. The JSON names match `worker.execution`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(rename = "timeoutSeconds", default = "defaults::timeout")]
    pub timeout_seconds: u64,
    #[serde(rename = "memoryMiB", default = "defaults::memory")]
    pub memory_mib: u64,
    #[serde(rename = "workspaceMiB", default = "defaults::workspace")]
    pub workspace_mib: u64,
    #[serde(default = "defaults::cpus")]
    pub cpus: u64,
    #[serde(default = "defaults::pids")]
    pub pids: u64,
    /// Attempts per review, counting setup and failures.
    #[serde(rename = "maxRuns", default = "defaults::runs")]
    pub max_runs: u64,
}
mod defaults {
    pub fn timeout() -> u64 {
        300
    }
    pub fn memory() -> u64 {
        2048
    }
    pub fn workspace() -> u64 {
        1024
    }
    pub fn cpus() -> u64 {
        2
    }
    pub fn pids() -> u64 {
        256
    }
    pub fn runs() -> u64 {
        12
    }
    pub fn podman() -> String {
        "podman".into()
    }
}
impl Default for Limits {
    fn default() -> Self {
        serde_json::from_value(json!({})).expect("defaults")
    }
}
impl Limits {
    fn check(&self) -> Result<()> {
        for (name, value, min, max) in [
            ("timeoutSeconds", self.timeout_seconds, 10, 3600),
            ("memoryMiB", self.memory_mib, 256, 65536),
            ("workspaceMiB", self.workspace_mib, 64, 8192),
            ("cpus", self.cpus, 1, 64),
            ("pids", self.pids, 32, 8192),
            ("maxRuns", self.max_runs, 1, 50),
        ] {
            ensure!(
                (min..=max).contains(&value),
                "worker.execution {name} must be between {min} and {max}"
            );
        }
        // Scratch space lives in memory-backed tmpfs.
        ensure!(
            self.workspace_mib < self.memory_mib,
            "worker.execution workspaceMiB must be smaller than memoryMiB"
        );
        Ok(())
    }
}

/// `worker.execution`: repositories where experiments may run, with optional limits.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "defaults::podman")]
    podman: String,
    #[serde(default)]
    repositories: BTreeMap<String, Limits>,
}
fn parse(value: &Value) -> Result<Config> {
    let config: Config = serde_json::from_value(value.clone()).context(
        "worker.execution must be {\"repositories\":{\"owner/repo\":{...}}} with optional \"podman\"",
    )?;
    ensure!(
        !config.podman.trim().is_empty(),
        "worker.execution podman must not be empty"
    );
    for (name, limits) in &config.repositories {
        crate::util::repo_name(name)
            .with_context(|| format!("Invalid repository in worker.execution: {name}"))?;
        limits.check()?;
    }
    Ok(config)
}
pub fn validate(value: &Value) -> Result<()> {
    parse(value).map(|_| ())
}
/// Whether this worker runs experiments for any repository.
pub fn configured(worker: &Value) -> bool {
    worker
        .get("execution")
        .and_then(|value| parse(value).ok())
        .is_some_and(|config| !config.repositories.is_empty())
}
/// The policy for one review, or `None` when experiments are not allowed. The
/// worker's own list decides; the service can only withhold execution, for
/// example for fork PRs or repositories open to every author.
pub fn resolve(worker: &Value, repo: &str, allowed_by_service: bool) -> Result<Option<Value>> {
    let Some(value) = worker.get("execution") else {
        return Ok(None);
    };
    let config = parse(value)?;
    let limits = config
        .repositories
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(repo))
        .map(|(_, limits)| limits);
    Ok(match limits {
        Some(limits) if allowed_by_service => {
            Some(json!({"podman": config.podman, "limits": limits}))
        }
        _ => None,
    })
}
/// The longest a runtime tool call can take, for provider MCP timeouts.
pub fn tool_timeout_seconds(policy: &Value) -> u64 {
    policy["limits"]["timeoutSeconds"]
        .as_u64()
        .unwrap_or_else(defaults::timeout)
        + 300
}

pub const TOOL_NAMES: &[&str] = &[
    "runtime_info",
    "prepare_environment",
    "run_experiment",
    "read_experiment",
];
pub fn tools() -> Vec<Value> {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        })
    };
    let revision = json!({
        "enum": ["head", "base"],
        "description": "head is the PR version; base is the merge base, before the PR.",
    });
    let command = json!({"type": "string", "description": "Shell command, run with /bin/sh -c in /workspace."});
    let purpose = json!({"type": "string", "description": "Short label for what this checks, shown in the review."});
    vec![
        tool(
            "runtime_info",
            "Show the sandbox's toolchains, limits, remaining attempts, prepared environments and earlier experiments. Call this first. Experiments run one at a time.",
            json!({}),
            &[],
        ),
        tool(
            "prepare_environment",
            "Install dependencies for one revision in a fresh container that can reach public package registries. On success the workspace is saved, and later run_experiment calls for that revision start from it. Counts as one attempt.",
            json!({"revision": revision, "command": command, "purpose": purpose}),
            &["revision", "command", "purpose"],
        ),
        tool(
            "run_experiment",
            "Run a command for one revision in a fresh container with no network. It starts from that revision's prepared environment if one exists, with the pinned source restored, unless fresh is true. Files it creates are discarded afterwards. Counts as one attempt.",
            json!({
                "revision": revision,
                "command": command,
                "purpose": purpose,
                "fresh": {"type": "boolean", "description": "Start from the plain source, ignoring a prepared environment."},
            }),
            &["revision", "command", "purpose"],
        ),
        tool(
            "read_experiment",
            "Read the saved receipt and output of an earlier experiment, for example after resuming a review.",
            json!({"id": {"type": "string"}}),
            &["id"],
        ),
    ]
}

/// Runtime tools inside the inspection MCP server for one review.
pub struct Execution {
    podman: String,
    env: BTreeMap<String, String>,
    limits: Limits,
    review: String,
    dir: PathBuf,
    source: Value,
    recovered: tokio::sync::OnceCell<()>,
    turn: tokio::sync::Mutex<()>,
}
impl Execution {
    /// Built from the review context; `None` when this review has no execution policy.
    pub fn from_context(context: &Value) -> Result<Option<Self>> {
        let policy = &context["job"]["settings"]["execution"];
        if !policy.is_object() || context["job"]["delegated"] == true {
            return Ok(None);
        }
        let limits: Limits =
            serde_json::from_value(policy["limits"].clone()).context("Invalid execution limits")?;
        limits.check()?;
        let review = context["job"]["id"]
            .as_str()
            .filter(|id| crate::retention::valid_id(id))
            .context("Invalid review ID")?
            .to_owned();
        let root = PathBuf::from(context["root"].as_str().context("Missing review root")?);
        Ok(Some(Self {
            podman: policy["podman"].as_str().unwrap_or("podman").to_owned(),
            env: podman_env(),
            limits,
            dir: root.join("reviews").join(&review).join("experiments"),
            review,
            source: context["source"].clone(),
            recovered: tokio::sync::OnceCell::new(),
            turn: tokio::sync::Mutex::new(()),
        }))
    }
    pub async fn call(&self, name: &str, args: &Value) -> Result<Value> {
        self.recovered.get_or_try_init(|| self.recover()).await?;
        let setup = match name {
            "runtime_info" => return self.info().await,
            "read_experiment" => return self.read(args),
            "prepare_environment" => true,
            "run_experiment" => false,
            _ => bail!("Unknown runtime tool"),
        };
        // Experiments run one at a time per review. The client's timeout covers
        // one experiment, so a second one fails now instead of waiting.
        let Ok(_turn) = self.turn.try_lock() else {
            bail!(
                "Another experiment is still running. Wait for its result, then start the next one."
            );
        };
        self.experiment(args, setup).await
    }
    /// Mark attempts that a crash or cancellation interrupted, and remove their containers.
    async fn recover(&self) -> Result<()> {
        crate::util::private_dir(&self.dir)?;
        for mut receipt in self.receipts()? {
            if receipt["status"] == "running" {
                receipt["status"] = json!("interrupted");
                receipt["error"] = json!("The review stopped before this attempt finished.");
                self.save(&receipt)?;
            }
        }
        sandbox::remove_review_containers(&self.podman, &self.env, &self.review).await
    }
    fn receipts(&self) -> Result<Vec<Value>> {
        let mut receipts = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(receipts),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                receipts.push(crate::util::read_json(&path)?.context("Empty receipt")?);
            }
        }
        receipts.sort_by_key(|r| r["id"].as_str().and_then(|id| id.parse::<u64>().ok()));
        Ok(receipts)
    }
    fn save(&self, receipt: &Value) -> Result<()> {
        let id: u64 = receipt["id"].as_str().context("Receipt ID")?.parse()?;
        crate::util::atomic(&self.dir.join(format!("{id:04}.json")), receipt)
    }
    fn environment(&self, revision: &str) -> PathBuf {
        self.dir
            .join("environments")
            .join(format!("{revision}.tar"))
    }
    async fn info(&self) -> Result<Value> {
        let receipts = self.receipts()?;
        let ready = image::exists(&self.podman, &self.env)
            .await
            .unwrap_or(false);
        let environments: BTreeMap<_, _> = ["head", "base"]
            .into_iter()
            .filter(|revision| self.environment(revision).is_file())
            .map(|revision| {
                let prepared = receipts.iter().rev().find(|r| {
                    r["kind"] == "setup" && r["revision"] == revision && r["status"] == "passed"
                });
                (revision, prepared.map(|r| r["purpose"].clone()))
            })
            .collect();
        let experiments: Vec<_> = receipts
            .iter()
            .map(|r| {
                json!({"id": r["id"], "kind": r["kind"], "revision": r["revision"], "purpose": r["purpose"], "status": r["status"], "exitCode": r["exitCode"]})
            })
            .collect();
        Ok(json!({
            "imageReady": ready,
            "toolchains": image::TOOLCHAINS,
            "user": "non-root (uid 1000); HOME is /workspace/.home",
            "writablePaths": ["/workspace", "/tmp"],
            "limits": self.limits,
            "attempts": {"used": receipts.len(), "remaining": self.limits.max_runs.saturating_sub(receipts.len() as u64)},
            "preparedEnvironments": environments,
            "experiments": experiments,
            "notes": [
                "Setup can reach public package registries; tests have no network.",
                "Environment variables exported during setup do not persist into tests.",
                "Compare equivalent commands on head and base before attributing a failure to the PR.",
            ],
        }))
    }
    fn read(&self, args: &Value) -> Result<Value> {
        check_args(args, &["id"])?;
        let id = args["id"].as_str().context("id must be a string")?;
        self.receipts()?
            .into_iter()
            .find(|r| r["id"] == id)
            .context("Unknown experiment ID")
    }
    async fn experiment(&self, args: &Value, setup: bool) -> Result<Value> {
        let allowed: &[&str] = if setup {
            &["revision", "command", "purpose"]
        } else {
            &["revision", "command", "purpose", "fresh"]
        };
        check_args(args, allowed)?;
        let revision = match args["revision"].as_str() {
            Some(revision @ ("head" | "base")) => revision,
            _ => bail!("revision must be head or base"),
        };
        let command = bounded_text(&args["command"], "command", 16_000)?;
        let purpose = bounded_text(&args["purpose"], "purpose", 200)?.replace(['\n', '\r'], " ");
        ensure!(
            args.get("fresh").is_none_or(Value::is_boolean),
            "fresh must be true or false"
        );
        let receipts = self.receipts()?;
        ensure!(
            (receipts.len() as u64) < self.limits.max_runs,
            "This review has used all {} experiment attempts. Continue with inspection.",
            self.limits.max_runs
        );
        ensure!(
            image::exists(&self.podman, &self.env).await?,
            "Crow's runtime image is not ready yet; the worker builds it in the background. Continue with inspection."
        );
        let commit = crate::inspection::revision(
            self.source[revision]
                .as_str()
                .context("Missing pinned revision")?,
        )?
        .to_owned();
        let id = (receipts.len() + 1).to_string();
        let mut receipt = json!({
            "id": id,
            "kind": if setup { "setup" } else { "test" },
            "revision": revision,
            "commit": commit,
            "purpose": purpose,
            "command": command,
            "status": "running",
            "startedAt": crate::util::now(),
        });
        self.save(&receipt)?;
        let started = std::time::Instant::now();
        let environment = self.environment(revision);
        let prepared = !setup && args["fresh"] != true && environment.is_file();
        let outcome = async {
            let source = self.dir.join(format!(".source-{id}.tar"));
            let cleanup = Temporary(source.clone());
            crate::inspection::execution_archive(
                &self.source,
                revision,
                &source,
                self.limits.workspace_mib * 1024 * 1024,
                CancellationToken::new(),
            )
            .await?;
            if setup {
                crate::util::private_dir(environment.parent().unwrap())?;
            }
            let run = sandbox::Run {
                name: format!("crow-{}-{id}", self.review),
                review: &self.review,
                image: &image::tag(),
                scratch: &self.dir,
                workspace: if prepared { &environment } else { &source },
                restore: prepared.then_some(source.as_path()),
                command: &command,
                snapshot: setup.then_some(environment.as_path()),
            };
            let sandbox = sandbox::Sandbox {
                podman: &self.podman,
                env: &self.env,
                limits: &self.limits,
            };
            let outcome = sandbox.run(run, &CancellationToken::new()).await;
            drop(cleanup);
            Ok::<_, anyhow::Error>(outcome)
        }
        .await;
        let outcome = outcome.unwrap_or_else(|e| sandbox::Outcome {
            status: "error",
            error: Some(format!("{e:#}")),
            ..Default::default()
        });
        merge(&mut receipt, sandbox::outcome_json(&outcome));
        receipt["durationMs"] = json!(started.elapsed().as_millis() as u64);
        receipt["preparedEnvironment"] = json!(prepared);
        self.save(&receipt)?;
        Ok(receipt)
    }
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) {
        target.extend(extra);
    }
}
fn check_args(args: &Value, allowed: &[&str]) -> Result<()> {
    let args = args.as_object().context("Arguments must be an object")?;
    if let Some(key) = args.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("Unsupported argument: {key}");
    }
    Ok(())
}
fn bounded_text(value: &Value, name: &str, max: usize) -> Result<String> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= max)
        .map(str::to_owned)
        .with_context(|| format!("{name} needs 1–{max} characters"))
}

/// Environment for Podman: the operator's own session, so rootless storage and
/// the user's systemd cgroup manager are found.
pub fn podman_env() -> BTreeMap<String, String> {
    crate::util::host_env().into_iter().collect()
}
pub(crate) struct PodmanOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}
pub(crate) async fn podman(
    executable: &str,
    env: &BTreeMap<String, String>,
    args: &[&str],
    require_success: bool,
) -> Result<PodmanOutput> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let output = sandbox::execute(executable, env, &args, None).await?;
    ensure!(
        !require_success || output.success,
        "podman {} failed: {}",
        args.first().map_or("", String::as_str),
        output.stderr.trim()
    );
    Ok(PodmanOutput {
        success: output.success,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// A short account of what ran, appended to the published review.
pub fn summary(review_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(review_dir.join("experiments")).ok()?;
    let mut receipts: Vec<Value> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .filter_map(|entry| crate::util::read_json(&entry.path()).ok().flatten())
        .collect();
    if receipts.is_empty() {
        return None;
    }
    receipts.sort_by_key(|r| r["id"].as_str().and_then(|id| id.parse::<u64>().ok()));
    let count = |kind: &str| {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for receipt in receipts.iter().filter(|r| r["kind"] == kind) {
            *counts
                .entry(receipt["status"].as_str().unwrap_or("unknown"))
                .or_default() += 1;
        }
        counts
            .iter()
            .map(|(status, n)| format!("{n} {status}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines =
        vec!["**Runtime checks** (sandboxed; failures include investigation attempts)".to_owned()];
    for (label, kind) in [("Tests", "test"), ("Setup", "setup")] {
        let counts = count(kind);
        if !counts.is_empty() {
            lines.push(format!("{label}: {counts}."));
        }
    }
    const SHOWN: usize = 10;
    for receipt in receipts.iter().filter(|r| r["kind"] == "test").take(SHOWN) {
        let version = if receipt["revision"] == "base" {
            "before the PR"
        } else {
            "PR version"
        };
        let purpose: String = receipt["purpose"]
            .as_str()
            .unwrap_or("")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(120)
            .collect();
        lines.push(format!(
            "- {}, {version}: {purpose}",
            receipt["status"].as_str().unwrap_or("unknown")
        ));
    }
    Some(lines.join("\n"))
}

/// Drop a finished review's prepared workspaces; receipts stay for retention.
pub fn release_environments(review_dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(review_dir.join("experiments").join("environments")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Worker maintenance: build the runtime image if it is missing. Containers need
/// no sweep: each is removed after its command, Podman stops and removes it when
/// its lifetime ends even if Crow has exited, and a resumed review removes its own.
pub async fn maintain(worker: &Value, cancel: &CancellationToken) -> Result<()> {
    if !configured(worker) {
        return Ok(());
    }
    let config = parse(&worker["execution"])?;
    let env = podman_env();
    if !image::exists(&config.podman, &env).await? {
        image::build(&config.podman, &env, cancel).await?;
    }
    Ok(())
}

/// `crow doctor --runtime` checks, or `None` when execution is not configured.
pub async fn diagnostics(worker: &Value) -> Result<Option<Value>> {
    if !configured(worker) {
        return Ok(None);
    }
    let config = parse(&worker["execution"])?;
    let env = podman_env();
    let mut checks = Vec::new();
    let info = podman(&config.podman, &env, &["info", "--format=json"], true).await;
    let host = match info.and_then(|o| Ok(serde_json::from_str::<Value>(&o.stdout)?)) {
        Ok(info) => info["host"].clone(),
        Err(e) => {
            checks.push(json!({"name":"Rootless Podman","ok":false,"detail":format!("{e:#}")}));
            return Ok(Some(json!({"ok": false, "checks": checks})));
        }
    };
    let rootless = host["security"]["rootless"] == true;
    checks.push(json!({"name":"Rootless Podman","ok":rootless,"detail":if rootless {"Podman runs as this user."} else {"Crow requires rootless Podman; run it as your normal user."}}));
    let controllers: Vec<&str> = host["cgroupControllers"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing: Vec<_> = ["cpu", "memory", "pids"]
        .into_iter()
        .filter(|c| !controllers.contains(c))
        .collect();
    let cgroups = host["cgroupVersion"] == "v2" && missing.is_empty();
    checks.push(json!({
        "name": "Resource limits",
        "ok": cgroups,
        "detail": if cgroups {
            "cgroup v2 with cpu, memory and pids delegated to this user.".to_owned()
        } else {
            format!("Needs cgroup v2 with cpu, memory and pids delegated to this user; missing: {}", missing.join(", "))
        },
    }));
    let seccomp = seccomp::profile(&config.podman, &env).await;
    checks.push(json!({
        "name": "Namespace blocking",
        "ok": seccomp.is_ok(),
        "detail": match seccomp {
            Ok(_) => "Crow's seccomp profile, derived from Podman's default, blocks new namespaces.".to_owned(),
            Err(e) => format!("{e:#}"),
        },
    }));
    let ready = image::exists(&config.podman, &env).await.unwrap_or(false);
    checks.push(json!({
        "name": "Runtime image",
        "ok": true,
        "detail": if ready { "Built.".to_owned() } else { format!("Not built yet; the worker builds {} in the background.", image::tag()) },
    }));
    let ok = checks.iter().all(|c| c["ok"] == true);
    Ok(Some(json!({"ok": ok, "checks": checks})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_explicit_per_repository() {
        assert!(!configured(&json!({})));
        assert!(!configured(&json!({"execution":{"repositories":{}}})));
        let worker = json!({"execution":{"repositories":{"Owner/Repo":{"timeoutSeconds":600}}}});
        assert!(configured(&worker));
        validate(&worker["execution"]).unwrap();
        for bad in [
            json!({"automatic": true}),
            json!({"repositories": {"not a repo": {}}}),
            json!({"repositories": {"o/r": {"cpus": 0}}}),
            json!({"repositories": {"o/r": {"memoryMiB": 512, "workspaceMiB": 1024}}}),
            json!({"repositories": {"o/r": {"image": "sha256:abc"}}}),
            json!({"podman": ""}),
        ] {
            assert!(validate(&bad).is_err(), "accepted {bad}");
        }
    }
    #[test]
    fn service_can_withhold_but_not_grant_execution() {
        let worker = json!({"execution":{"repositories":{"Owner/Repo":{"maxRuns":3}}}});
        let policy = resolve(&worker, "owner/repo", true).unwrap().unwrap();
        assert_eq!(policy["limits"]["maxRuns"], 3);
        assert_eq!(policy["limits"]["timeoutSeconds"], defaults::timeout());
        assert_eq!(policy["podman"], "podman");
        assert!(resolve(&worker, "owner/repo", false).unwrap().is_none());
        assert!(resolve(&worker, "owner/other", true).unwrap().is_none());
        assert!(resolve(&json!({}), "owner/repo", true).unwrap().is_none());
        assert_eq!(tool_timeout_seconds(&policy), defaults::timeout() + 300);
    }
    #[test]
    fn tool_definitions_match_their_names() {
        let names: Vec<_> = tools().iter().map(|t| t["name"].clone()).collect();
        assert_eq!(
            names,
            TOOL_NAMES.iter().map(|n| json!(n)).collect::<Vec<_>>()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_stops_a_runtime_image_build() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let podman = dir.path().join("podman");
        // The image is missing, and building it never finishes.
        std::fs::write(
            &podman,
            "#!/bin/sh\n[ \"$1\" = build ] && exec sleep 600\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
        let worker = json!({"execution": {"podman": podman, "repositories": {"o/r": {}}}});
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stop.cancel();
        });
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            maintain(&worker, &cancel),
        )
        .await
        .expect("cancelling must stop the build");
        assert!(stopped.is_err());
    }
    #[test]
    fn delegated_reviews_never_receive_execution() {
        let context = json!({"root":"/tmp","job":{"id":"r1","delegated":true,"settings":{"execution":{"podman":"podman","limits":Limits::default()}}},"source":{}});
        assert!(Execution::from_context(&context).unwrap().is_none());
        let mut parent = context.clone();
        parent["job"]["delegated"] = json!(false);
        assert!(Execution::from_context(&parent).unwrap().is_some());
    }
    #[test]
    fn summary_counts_outcomes_and_names_versions() {
        let dir = tempfile::tempdir().unwrap();
        let experiments = dir.path().join("experiments");
        for (id, kind, revision, status, purpose) in [
            ("1", "setup", "head", "passed", "Install"),
            (
                "2",
                "test",
                "head",
                "failed",
                "Run cart tests\nwith a newline",
            ),
            ("3", "test", "base", "passed", "Run cart tests"),
        ] {
            crate::util::atomic(
                &experiments.join(format!("{id}.json")),
                &json!({"id":id,"kind":kind,"revision":revision,"status":status,"purpose":purpose}),
            )
            .unwrap();
        }
        let text = summary(dir.path()).unwrap();
        assert!(text.contains("Tests: 1 failed, 1 passed."), "{text}");
        assert!(text.contains("Setup: 1 passed."));
        assert!(text.contains("- failed, PR version: Run cart tests"));
        assert!(text.contains("- passed, before the PR: Run cart tests"));
        assert!(summary(tempfile::tempdir().unwrap().path()).is_none());
    }
}
