//! Operator-authorized experiments in disposable rootless Podman containers.
use crate::runtime_diagnostics::{Stage, Trace, bounded_error};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    #[serde(default = "auto_image")]
    pub image: String,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "memory")]
    pub memory_mi_b: u64,
    #[serde(default = "workspace")]
    pub workspace_mi_b: u64,
    #[serde(default = "cpus")]
    pub cpus: u64,
    #[serde(default = "pids")]
    pub pids: u64,
    #[serde(default = "runs")]
    pub max_runs: u64,
}
fn auto_image() -> String {
    "auto".into()
}
fn timeout() -> u64 {
    120
}
fn memory() -> u64 {
    1024
}
fn workspace() -> u64 {
    512
}
fn cpus() -> u64 {
    2
}
fn pids() -> u64 {
    128
}
fn runs() -> u64 {
    12
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "podman")]
    podman: String,
    #[serde(default)]
    repositories: BTreeMap<String, Policy>,
    #[serde(default)]
    automatic: bool,
}
fn podman() -> String {
    "podman".into()
}
fn config(value: &Value) -> Result<Config> {
    let mut config: Config = if value.is_null() {
        Config {
            podman: podman(),
            ..Default::default()
        }
    } else {
        serde_json::from_value(value.clone()).context("Invalid worker.execution configuration")?
    };
    ensure!(
        !config.podman.is_empty(),
        "execution.podman must name the Podman executable"
    );
    for (repo, policy) in std::mem::take(&mut config.repositories) {
        let repo = crate::util::repo_name(&repo).context("Invalid execution repository name")?;
        ensure!(
            policy.image == "auto"
                || policy
                    .image
                    .strip_prefix("sha256:")
                    .is_some_and(|s| s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "Execution image must be auto or a local immutable sha256 image ID"
        );
        ensure!(
            (1..=1800).contains(&policy.timeout_seconds),
            "Execution timeoutSeconds must be 1–1800"
        );
        ensure!(
            (64..=32768).contains(&policy.memory_mi_b),
            "Execution memoryMiB must be 64–32768"
        );
        ensure!(
            (16..=16384).contains(&policy.workspace_mi_b),
            "Execution workspaceMiB must be 16–16384"
        );
        ensure!(
            (1..=32).contains(&policy.cpus) && (16..=1024).contains(&policy.pids),
            "Invalid execution CPU or process limit"
        );
        ensure!(
            (1..=50).contains(&policy.max_runs),
            "Execution maxRuns must be 1–50"
        );
        ensure!(
            config.repositories.insert(repo.clone(), policy).is_none(),
            "Duplicate execution repository policy after case normalization: {repo}"
        );
    }
    Ok(config)
}
pub fn validate(value: &Value) -> Result<()> {
    config(value).map(|_| ())
}
pub fn enabled(settings: &Value, repo: &str) -> Result<bool> {
    let cfg = config(&settings["execution"])?;
    Ok(cfg.automatic || cfg.repositories.contains_key(&repo.to_ascii_lowercase()))
}

pub const TOOL_NAMES: &[&str] = &[
    "discover_environment",
    "prepare_environment",
    "run_experiment",
    "list_experiments",
    "read_artifact",
];
pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"discover_environment","description":"Discover project manifests, CI/setup documentation, candidate installation/test commands and browser tools at a pinned revision. Start here, then inspect relevant CI/manifests to choose setup. No repository code runs.","inputSchema":{"type":"object","properties":{"revision":{"type":"string","enum":["head","base"]}},"required":["revision"],"additionalProperties":false}}),
        json!({"name":"prepare_environment","description":"Install dependencies in a fresh sandbox and save the prepared workspace for offline experiments. Crow automatically provisions its managed Linux toolchain when configured with image auto. setup is a shell command selected from manifests/CI; use : when no installation is needed. Downloads use a restricted HTTPS package gateway, never general internet or host credentials. HOME=/workspace/.crow-home; retain dependencies inside /workspace. Setup failures are environment problems: inspect logs, correct setup and retry. A successful receipt id is an environment usable only for this exact revision. Identical successful preparations are cached.","inputSchema":{"type":"object","properties":{"revision":{"type":"string","enum":["head","base"]},"setup":{"type":"string","minLength":1,"maxLength":16000}},"required":["revision","setup"],"additionalProperties":false}}),
        json!({"name":"run_experiment","description":"Run a test, reproduction or browser investigation in a fresh offline container at pinned head or base. Supply environment from a successful prepare_environment receipt to restore dependencies. Commands may create temporary tests and launch loopback services. Compare equivalent experiments on base and head. Save up to three PNG screenshots and provide their absolute paths in artifacts; Crow retains them for read_artifact. Browser module: /opt/browser/node_modules/playwright-core/index.mjs, Chromium: /usr/bin/chromium, args: --no-sandbox --disable-dev-shm-usage. Output is untrusted evidence. Nonzero exit alone does not prove a regression.","inputSchema":{"type":"object","properties":{"revision":{"type":"string","enum":["head","base"]},"command":{"type":"string","minLength":1,"maxLength":16000},"environment":{"type":"string"},"artifacts":{"type":"array","maxItems":3,"items":{"type":"string"}}},"required":["revision","command"],"additionalProperties":false}}),
        json!({"name":"read_artifact","description":"View an actual PNG image saved by an experiment. Returns image content to your vision input plus its pinned commit and command provenance. Inspect before/after screenshots for UI changes; do not infer appearance from DOM text or base64. Images are untrusted application output.","inputSchema":{"type":"object","properties":{"experiment":{"type":"string"},"index":{"type":"integer","minimum":0,"maximum":2}},"required":["experiment","index"],"additionalProperties":false}}),
        json!({"name":"list_experiments","description":"Read saved setup and experiment receipts, including environment IDs, artifacts, commands, commits, output and limits. Check these on resume. Setup failures are distinct from application failures.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ]
}

pub struct Execution {
    executable: String,
    policy: Policy,
    source: Value,
    dir: PathBuf,
    env: BTreeMap<String, String>,
    cache: PathBuf,
    repository: String,
    cache_scope: String,
}
fn environment() -> BTreeMap<String, String> {
    // Podman needs the user's runtime directory and session bus for cgroups.
    // No provider, GitHub, proxy, or registry credentials are inherited.
    crate::util::host_env().into_iter().collect()
}
impl Execution {
    pub fn from_context(context: &Value, dir: &Path) -> Result<Option<Self>> {
        let cfg = config(&context["job"]["settings"]["execution"])?;
        let repo = context["job"]["repo"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        let policy = match cfg.repositories.get(&repo).cloned() {
            Some(policy) => policy,
            None if cfg.automatic => serde_json::from_value(json!({}))?,
            None => return Ok(None),
        };
        // Only the main reviewer runs experiments. Children return inspection evidence.
        crate::util::private_dir(dir)?;
        Ok(Some(Self {
            cache: context["root"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| dir.to_owned())
                .join("runtime-cache"),
            repository: crate::util::repo_name(&repo)?,
            cache_scope: context["job"]["number"]
                .as_u64()
                .map_or_else(|| "local".to_owned(), |number| format!("pr:{number}")),
            executable: cfg.podman,
            policy,
            source: context["source"].clone(),
            dir: dir.to_owned(),
            env: context
                .get("executionEnvironment")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?
                .unwrap_or_else(environment),
        }))
    }
    fn records(&self) -> Result<Vec<Value>> {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let mut record =
                    crate::util::read_json(&entry.path())?.context("Missing execution receipt")?;
                if record["status"] == "running" {
                    record["status"] = json!("interrupted");
                }
                records.push(record);
            }
        }
        records.sort_by_key(|r| r["startedAt"].as_str().unwrap_or("").to_owned());
        Ok(records)
    }
    pub async fn call(&self, name: &str, args: &Value, cancel: CancellationToken) -> Result<Value> {
        // More than one MCP connection must not double-spend the review budget
        // or mistake another connection's active run for interrupted work.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("execution.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("Another experiment is still running")?;
        self.recover().await?;
        if name == "list_experiments" {
            return Ok(json!({"policy":self.policy,"runs":self.records()?}));
        }
        if name == "discover_environment" {
            let revision = requested_revision(args)?;
            return crate::runtime::discover(&self.source, revision).await;
        }
        if name == "read_artifact" {
            return self.read_artifact(args);
        }
        let prepare = name == "prepare_environment";
        ensure!(
            prepare || name == "run_experiment",
            "Unknown execution tool"
        );
        ensure!(
            args.is_object()
                && args.as_object().unwrap().keys().all(|k| if prepare {
                    ["revision", "setup"].contains(&k.as_str())
                } else {
                    ["revision", "command", "environment", "artifacts"].contains(&k.as_str())
                }),
            "Invalid experiment arguments"
        );
        let key = requested_revision(args)?;
        let script = args[if prepare { "setup" } else { "command" }]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 16000 && !s.contains('\0'))
            .context("Command must contain 1–16000 bytes without NUL")?;
        let artifacts: Vec<String> = args
            .get("artifacts")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()?
            .unwrap_or_default();
        ensure!(
            artifacts.len() <= 3
                && artifacts
                    .iter()
                    .all(|p| p.starts_with('/') && p.len() <= 1024 && !p.contains('\0')),
            "Provide up to three absolute PNG paths"
        );
        ensure!(
            self.records()?.len() < self.policy.max_runs as usize,
            "This review's experiment budget is exhausted"
        );
        let commit = crate::inspection::revision(
            self.source[key]
                .as_str()
                .context("Missing pinned revision")?,
        )?;
        let environment = args
            .get("environment")
            .map(|v| v.as_str().context("Invalid environment ID"))
            .transpose()?;
        let restored = environment
            .map(|id| self.prepared(id, commit))
            .transpose()?;
        let id = crate::util::id();
        let name = format!("crow-experiment-{id}");
        let path = self.dir.join(format!("{id}.json"));
        let mut record = json!({"id":id,"revision":key,"commit":commit,"image":self.policy.image,"command":script,"phase":if prepare {"setup"} else {"test"},"environment":environment,"limits":self.policy,"status":"running","startedAt":chrono::Utc::now().to_rfc3339(),"exitCode":null,"containerStarted":false});
        crate::util::atomic(&path, &record)?;
        let trace = Trace::new(&path);
        let start = Instant::now();
        let provision = async {
            trace.set(Stage::RuntimeCheck)?;
            check_runtime(&self.executable, &self.env).await?;
            let image = if self.policy.image == "auto" {
                trace.set(Stage::ImageProvision)?;
                crate::runtime::image(
                    &self.executable,
                    &self.env,
                    &self.cache,
                    cancel.clone(),
                    1800,
                )
                .await?
            } else {
                self.policy.image.clone()
            };
            if let Some((_, prepared_image)) = &restored {
                ensure!(
                    *prepared_image == image,
                    "Execution image changed; prepare the environment again"
                );
            }
            Ok::<_, anyhow::Error>(image)
        };
        let image = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Environment provisioning interrupted")),
            _ = tokio::time::sleep(Duration::from_secs(1800)) => Err(anyhow::anyhow!("Environment provisioning timed out")),
            result = provision => result,
        };
        record["provisionMs"] = json!(start.elapsed().as_millis() as u64);
        let mut guard = ContainerGuard {
            executable: self.executable.clone(),
            name: name.clone(),
            env: self.env.clone(),
            armed: true,
            started: trace.container_started(),
        };
        let live = LiveOutput::default();
        let completed = std::sync::Mutex::new(None::<Value>);
        let operation = async {
            let image = image?;
            record["image"] = json!(image);
            crate::util::private_dir(&self.dir.join("environments"))?;
            let snapshot = self.dir.join("environments").join(format!("{id}.tar"));
            // Whole prepared workspaces are reusable only within this review.
            // Hard links avoid another 512 MiB copy for identical preparation attempts.
            if prepare {
                for previous in self.records()? {
                    if previous["phase"] != "setup"
                        || previous["status"] != "passed"
                        || previous["commit"] != commit
                        || previous["image"] != image
                        || previous["command"] != script
                    {
                        continue;
                    }
                    let Some(previous_id) = previous["id"].as_str() else {
                        continue;
                    };
                    if let Ok((existing, _)) = self.prepared(previous_id, commit) {
                        trace.set(Stage::CacheRestore)?;
                        if std::fs::hard_link(&existing, &snapshot).is_err() {
                            std::fs::copy(existing, &snapshot)?;
                        }
                        return Ok::<_, anyhow::Error>(
                            json!({"status":"passed","exitCode":0,"cached":true,"stdout":"Reused an identical preparation from this review.","stderr":"","outputTruncated":false}),
                        );
                    }
                }
            }
            let package_plan = if prepare {
                match crate::runtime_cache::plan(
                    &self.source,
                    key,
                    &self.repository,
                    &self.cache_scope,
                    &image,
                )
                .await
                {
                    Ok(plan) => plan,
                    Err(error) => {
                        record["cacheRestoreError"] = json!(bounded_error(error));
                        None
                    }
                }
            } else {
                None
            };
            let output = self
                .run(
                    &name,
                    key,
                    script,
                    &image,
                    restored.as_ref().map(|(path, _)| path.as_path()),
                    if prepare {
                        Some(snapshot.as_path())
                    } else {
                        None
                    },
                    &artifacts,
                    &id,
                    &live,
                    &trace,
                    package_plan.as_ref(),
                    &completed,
                )
                .await?;
            Ok(output)
        };
        let mut result = tokio::select! {
            _ = cancel.cancelled() => Ok(json!({"status":"interrupted","exitCode":null})),
            _ = tokio::time::sleep(Duration::from_secs(self.policy.timeout_seconds)) => Ok(json!({"status":"timed_out","exitCode":null})),
            result = operation => result,
        };
        if let Ok(interrupted) = &result
            && matches!(
                interrupted["status"].as_str(),
                Some("timed_out" | "interrupted")
            )
            && let Some(mut finished) = completed.lock().unwrap().take()
        {
            let last = crate::util::read_json(&path)?.unwrap_or_default();
            if last["stage"] == "cache_save" {
                finished["cacheSaveError"] = json!(
                    "Optional dependency cache save did not finish before the experiment stopped."
                );
                result = Ok(finished);
            } else if last["stage"] == "artifact_collection" {
                let mut saved = finished["artifacts"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for path in artifacts.iter().skip(saved.len()) {
                    saved.push(json!({"path":path,"saved":false,"error":"Screenshot collection did not finish before the experiment stopped."}));
                }
                finished["artifacts"] = json!(saved);
                result = Ok(finished);
            }
        }
        // Cleanup is awaited even on cancellation. An armed guard covers dropped futures.
        let cleanup = if trace.started() {
            remove_container(&self.executable, &name, &self.env).await
        } else {
            Ok(())
        };
        guard.armed = cleanup.is_err();
        match result {
            Ok(output) => {
                for (key, value) in output.as_object().context("Invalid experiment result")? {
                    record[key] = value.clone();
                }
            }
            Err(error) => {
                record["status"] = json!(if cancel.is_cancelled() {
                    "interrupted"
                } else {
                    "error"
                });
                record["error"] = json!(bounded_error(format!("{error:#}")));
            }
        }
        if record.get("stdout").is_none() {
            for (key, value) in live.value().as_object().unwrap() {
                record[key] = value.clone();
            }
        }
        if let Err(error) = cleanup {
            record["cleanupError"] = json!(bounded_error(error));
        }
        if prepare && record["status"] != "passed" {
            let _ = std::fs::remove_file(self.dir.join("environments").join(format!("{id}.tar")));
        }
        trace.finish(&mut record);
        record["durationMs"] = json!(start.elapsed().as_millis() as u64);
        crate::util::atomic(&path, &record)?;
        Ok(record)
    }
    fn prepared(&self, id: &str, commit: &str) -> Result<(PathBuf, String)> {
        validate_id(id)?;
        let receipt = crate::util::read_json(&self.dir.join(format!("{id}.json")))?
            .context("Unknown environment")?;
        ensure!(
            receipt["phase"] == "setup"
                && receipt["status"] == "passed"
                && receipt["commit"] == commit,
            "Environment must be a successful preparation of the exact requested commit"
        );
        let snapshot = self.dir.join("environments").join(format!("{id}.tar"));
        ensure!(
            snapshot.is_file(),
            "Prepared environment is missing; prepare it again"
        );
        Ok((
            snapshot,
            receipt["image"]
                .as_str()
                .context("Missing image")?
                .to_owned(),
        ))
    }
    fn read_artifact(&self, args: &Value) -> Result<Value> {
        use base64::Engine;
        let id = args["experiment"].as_str().context("Missing experiment")?;
        validate_id(id)?;
        let index = args["index"]
            .as_u64()
            .filter(|i| *i < 3)
            .context("Invalid artifact index")? as usize;
        let record = crate::util::read_json(&self.dir.join(format!("{id}.json")))?
            .context("Unknown experiment")?;
        ensure!(
            record["artifacts"][index]["saved"] == true,
            "Artifact was not captured"
        );
        let bytes = std::fs::read(self.dir.join("artifacts").join(format!("{id}-{index}.png")))?;
        validate_png(&bytes)?;
        Ok(
            json!({"content":[{"type":"text","text":json!({"experiment":id,"revision":record["revision"],"commit":record["commit"],"command":record["command"],"artifact":record["artifacts"][index]}).to_string()},{"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(bytes)}]}),
        )
    }
    async fn recover(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().is_none_or(|e| e != "json") {
                continue;
            }
            let mut record =
                crate::util::read_json(&entry.path())?.context("Missing experiment receipt")?;
            if record["status"] != "running" {
                continue;
            }
            let id = record["id"]
                .as_str()
                .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
                .context("Invalid experiment ID")?;
            // Old receipts lack this flag, so retain their conservative cleanup.
            if record["containerStarted"] != false {
                remove_container(
                    &self.executable,
                    &format!("crow-experiment-{id}"),
                    &self.env,
                )
                .await?;
            }
            record["status"] = json!("interrupted");
            if let Some(stage) = record.get("stage").cloned() {
                record["failureStage"] = stage;
            }
            crate::util::atomic(&entry.path(), &record)?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        name: &str,
        revision: &str,
        script: &str,
        image: &str,
        prepared: Option<&Path>,
        snapshot: Option<&Path>,
        artifacts: &[String],
        id: &str,
        live: &LiveOutput,
        trace: &Trace,
        package_plan: Option<&crate::runtime_cache::Plan>,
        completed: &std::sync::Mutex<Option<Value>>,
    ) -> Result<Value> {
        trace.set(Stage::SourceArchive)?;
        let archive = tempfile::Builder::new()
            .prefix(".crow-runtime-")
            .tempfile_in(&self.dir)?;
        if let Some(prepared) = prepared {
            std::fs::copy(prepared, archive.path())?;
        } else {
            crate::inspection::execution_archive(
                &self.source,
                revision,
                archive.path(),
                (self.policy.workspace_mi_b * 1024 * 1024).min(SNAPSHOT_LIMIT),
                CancellationToken::new(),
            )
            .await?;
        }
        let p = &self.policy;
        let gateway = if snapshot.is_some() {
            trace.set(Stage::GatewayStart)?;
            Some(crate::downloads::Gateway::start_in(&self.dir)?)
        } else {
            None
        };
        let mut args = vec![
            "run".into(),
            "--detach".into(),
            "--pull=never".into(),
            format!("--name={name}"),
            "--network=none".into(),
            "--read-only".into(),
            "--read-only-tmpfs=false".into(),
            "--image-volume=ignore".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--user=0:0".into(),
            "--pid=private".into(),
            "--ipc=private".into(),
            "--log-driver=none".into(),
            "--http-proxy=false".into(),
            "--env=HOME=/workspace/.crow-home".into(),
            "--env=CI=true".into(),
            format!("--memory={}m", p.memory_mi_b),
            format!("--memory-swap={}m", p.memory_mi_b),
            format!("--cpus={}", p.cpus),
            format!("--pids-limit={}", p.pids),
            format!("--timeout={}", p.timeout_seconds + 5),
            "--stop-timeout=1".into(),
            "--ulimit=nofile=1024:1024".into(),
            "--ulimit=core=0:0".into(),
            format!(
                "--tmpfs=/workspace:rw,exec,nosuid,nodev,size={}m",
                p.workspace_mi_b
            ),
            format!(
                "--tmpfs=/tmp:rw,exec,nosuid,nodev,size={}m",
                p.workspace_mi_b
            ),
            "--workdir=/workspace".into(),
            "--entrypoint=/bin/sh".into(),
        ];
        if let Some(gateway) = &gateway {
            args.push(format!(
                "--volume={}:/run/crow-downloads:ro,Z",
                gateway.directory().display()
            ));
        }
        args.extend([
            image.into(),
            "-c".into(),
            format!("exec sleep {}", p.timeout_seconds + 5),
        ]);
        trace.set(Stage::ContainerStart)?;
        crate::process::run(
            &self.executable,
            &args,
            crate::process::RunOptions {
                env: Some(self.env.clone()),
                timeout: Some(Duration::from_secs(20)),
                ..Default::default()
            },
        )
        .await?;
        if gateway.is_some() {
            trace.set(Stage::GatewayStart)?;
            // Relabel only this experiment's private socket directory. SELinux
            // also checks the listening process domain, so a volume label alone
            // does not prove that the host policy permits this connection.
            let checked = self
                .command(
                    &[
                        "exec",
                        name,
                        "/bin/sh",
                        "-c",
                        r#"# Crow package gateway preflight
command -v python3 >/dev/null && [ -r /opt/crow/proxy.py ] || { echo 'Crow setup requires python3 and /opt/crow/proxy.py in the execution image' >&2; exit 125; }
python3 -I -c 'import socket,sys
try:
 with socket.socket(socket.AF_UNIX) as client:
  client.settimeout(5)
  client.connect("/run/crow-downloads/socket")
except PermissionError:
 sys.exit("Crow package gateway socket access denied. Check host directory permissions and SELinux policy for container-to-worker Unix socket connections; Crow keeps container labeling enabled.")
except OSError as error:
 sys.exit("Crow package gateway socket is unavailable: " + str(error))'"#,
                    ],
                    None,
                    None,
                )
                .await?;
            ensure!(
                checked["status"] == "passed",
                "Package gateway connection preflight failed: {checked}"
            );
        }
        let restore = if prepared.is_some() {
            "tar --delay-directory-restore -xf - -C /workspace"
        } else {
            "tar -xf - -C /workspace"
        };
        let limits = format!(
            "read memory < /sys/fs/cgroup/memory.max && [ \"$memory\" = {} ] && read pids < /sys/fs/cgroup/pids.max && [ \"$pids\" = {} ] && read quota period < /sys/fs/cgroup/cpu.max && [ \"$quota\" != max ] && [ \"$quota\" -le \"$(({} * $period))\" ] || exit 125; {restore}",
            p.memory_mi_b * 1024 * 1024,
            p.pids,
            p.cpus
        );
        let mut cache_info = json!({});
        if let Some(plan) = package_plan {
            cache_info["packageCacheKey"] = json!(plan.key);
            trace.set(Stage::CacheRestore)?;
            let checked = self
                .command(
                    &[
                        "exec",
                        name,
                        "/bin/sh",
                        "-c",
                        limits.rsplit_once(';').unwrap().0,
                    ],
                    None,
                    None,
                )
                .await?;
            ensure!(
                checked["status"] == "passed",
                "Cannot enforce resource limits: {checked}"
            );
            let packages = tempfile::Builder::new()
                .prefix(".crow-runtime-packages-")
                .tempfile_in(&self.dir)?;
            let root = self.cache.clone();
            let key = plan.key.clone();
            let (packages, loaded) = tokio::task::spawn_blocking(move || {
                let loaded = crate::runtime_cache::load(&root, &key, packages.path());
                (packages, loaded)
            })
            .await?;
            match loaded {
                Ok(true) => {
                    let pins = plan.pins.to_string();
                    let workspace_bytes = self.policy.workspace_mi_b * 1024 * 1024;
                    let import_budget = workspace_bytes
                        .saturating_sub(archive.as_file().metadata()?.len())
                        .saturating_sub(workspace_bytes / 4)
                        .min(workspace_bytes / 4)
                        .min(128 * 1024 * 1024)
                        .to_string();
                    let imported = self
                        .command(
                            &[
                                "exec",
                                "--interactive",
                                name,
                                "python3",
                                "-I",
                                "-c",
                                include_str!("runtime/packages.py"),
                                "import",
                                &pins,
                                "/workspace",
                                &import_budget,
                            ],
                            Some(packages.path()),
                            None,
                        )
                        .await;
                    match imported {
                        Ok(result) if result["status"] == "passed" => {
                            let stats: Value =
                                serde_json::from_str(result["stdout"].as_str().unwrap_or("{}"))?;
                            cache_info["packageCacheRestored"] =
                                json!(stats["files"].as_u64().unwrap_or(0) > 0);
                            cache_info["packageCache"] = stats;
                        }
                        Ok(result) => {
                            cache_info["cacheRestoreError"] = json!(bounded_error(result))
                        }
                        Err(error) => cache_info["cacheRestoreError"] = json!(bounded_error(error)),
                    }
                }
                Ok(false) => {
                    cache_info["packageCacheRestored"] = json!(false);
                }
                Err(error) => {
                    cache_info["cacheRestoreError"] = json!(bounded_error(error));
                }
            }
        }
        trace.set(Stage::WorkspaceRestore)?;
        let extracted = self
            .command(
                &["exec", "--interactive", name, "/bin/sh", "-c", &limits],
                Some(archive.path()),
                None,
            )
            .await?;
        ensure!(
            extracted["status"] == "passed",
            "Cannot enforce limits or restore workspace: {extracted}"
        );
        // Installation hooks may edit tracked files. Always restore pinned source
        // over a dependency snapshot before testing; never silently test those edits.
        if prepared.is_some() {
            trace.set(Stage::SourceRestore)?;
            let source_archive = tempfile::Builder::new()
                .prefix(".crow-runtime-")
                .tempfile_in(&self.dir)?;
            crate::inspection::execution_archive(
                &self.source,
                revision,
                source_archive.path(),
                (self.policy.workspace_mi_b * 1024 * 1024).min(SNAPSHOT_LIMIT),
                CancellationToken::new(),
            )
            .await?;
            let restored = self
                .command(
                    &[
                        "exec",
                        "--interactive",
                        name,
                        "python3",
                        "-I",
                        "-c",
                        include_str!("runtime/restore.py"),
                    ],
                    Some(source_archive.path()),
                    None,
                )
                .await?;
            ensure!(
                restored["status"] == "passed",
                "Cannot restore pinned source over prepared dependencies: {restored}"
            );
        }
        let command = if gateway.is_some() {
            format!(
                "python3 -I /opt/crow/proxy.py >/tmp/crow-proxy.log 2>&1 &\nproxy=$!\ntrap 'kill $proxy 2>/dev/null || true' EXIT\nexport HTTPS_PROXY=http://127.0.0.1:3128 HTTP_PROXY=http://127.0.0.1:3128 https_proxy=http://127.0.0.1:3128 http_proxy=http://127.0.0.1:3128 NO_PROXY=127.0.0.1,localhost\nexport PIP_INDEX_URL=https://pypi.org/simple\nmkdir -p \"$HOME\"\npython3 -I -c 'import socket,time
for attempt in range(100):
 try:
  socket.create_connection((\"127.0.0.1\",3128),timeout=.1).close(); break
 except OSError: time.sleep(.05)
else: raise SystemExit(\"Crow dependency proxy did not start\")' || {{ cat /tmp/crow-proxy.log >&2; exit 125; }}\n/bin/sh -c {}",
                shell_quote(script)
            )
        } else {
            script.to_owned()
        };
        let command_stage = if snapshot.is_some() {
            Stage::SetupCommand
        } else {
            Stage::TestCommand
        };
        trace.set(command_stage)?;
        let mut output = self
            .command(&["exec", name, "/bin/sh", "-c", &command], None, Some(live))
            .await?;
        if output["status"] != "passed" {
            output["failureStage"] = json!(command_stage);
        }
        if let Some(gateway) = gateway {
            let errors = gateway.close().await;
            if !errors.is_empty() {
                output["gatewayErrors"] = json!(errors);
            }
        }
        if let Some(snapshot) = snapshot
            && output["status"] == "passed"
        {
            trace.set(Stage::SnapshotExport)?;
            // UID 0 has no DAC override capability here. Archive owner-writable
            // cache entries so tar can populate Go's read-only module directories.
            // Pinned tracked-file permissions are restored before each experiment.
            self.export(
                &[
                    "exec",
                    name,
                    "tar",
                    "--mode=u+rwX",
                    "-cf",
                    "-",
                    "-C",
                    "/workspace",
                    ".",
                ],
                snapshot,
                SNAPSHOT_LIMIT,
            )
            .await?;
        }
        // Once a command and its required snapshot finish, optional cache/artifact
        // work must not relabel the actual command outcome if the deadline fires.
        for (key, value) in cache_info.as_object().unwrap() {
            output[key] = value.clone();
        }
        *completed.lock().unwrap() = Some(output.clone());
        if let Some(plan) = package_plan
            && output["status"] == "passed"
        {
            trace.set(Stage::CacheSave)?;
            let save = async {
                let packages = tempfile::Builder::new()
                    .prefix(".crow-runtime-packages-")
                    .tempfile_in(&self.dir)?;
                let pins = plan.pins.to_string();
                self.export(
                    &[
                        "exec",
                        name,
                        "python3",
                        "-I",
                        "-c",
                        include_str!("runtime/packages.py"),
                        "export",
                        &pins,
                    ],
                    packages.path(),
                    crate::runtime_cache::ARCHIVE_LIMIT,
                )
                .await?;
                let root = self.cache.clone();
                let key = plan.key.clone();
                tokio::task::spawn_blocking(move || {
                    crate::runtime_cache::save(&root, &key, packages.path())
                })
                .await??;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = save {
                cache_info["cacheSaveError"] = json!(bounded_error(error));
            }
        }
        for (key, value) in cache_info.as_object().unwrap() {
            output[key] = value.clone();
        }
        *completed.lock().unwrap() = Some(output.clone());
        let mut saved = Vec::new();
        for (index, path) in artifacts.iter().enumerate() {
            trace.set(Stage::ArtifactCollection)?;
            let target = self.dir.join("artifacts").join(format!("{id}-{index}.png"));
            let result = async {
                crate::util::private_dir(&self.dir.join("artifacts"))?;
                self.export(
                    &["exec", name, "cat", "--", path],
                    &target,
                    ARTIFACT_LIMIT as u64,
                )
                .await?;
                let bytes = std::fs::read(&target)?;
                validate_png(&bytes)?;
                Ok::<_, anyhow::Error>(bytes.len())
            }
            .await;
            match result {
                Ok(size) => saved
                    .push(json!({"path":path,"saved":true,"bytes":size,"mimeType":"image/png"})),
                Err(e) => {
                    let _ = std::fs::remove_file(&target);
                    saved.push(json!({"path":path,"saved":false,"error":bounded_error(e)}));
                }
            }
            output["artifacts"] = json!(saved);
            *completed.lock().unwrap() = Some(output.clone());
        }
        output["artifacts"] = json!(saved);
        Ok(output)
    }
    async fn command(
        &self,
        args: &[&str],
        input: Option<&Path>,
        live: Option<&LiveOutput>,
    ) -> Result<Value> {
        let mut child = Command::new(&self.executable)
            .args(args)
            .env_clear()
            .envs(&self.env)
            .stdin(match input {
                Some(p) => Stdio::from(std::fs::File::open(p)?),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let live = live.cloned().unwrap_or_default();
        let (_, _, status) = tokio::try_join!(
            capture_shared(out, &live.stdout),
            capture_shared(err, &live.stderr),
            child.wait()
        )?;
        let code = status.code();
        let outcome = if status.success() {
            "passed"
        } else if matches!(code, Some(125..=127)) {
            "error"
        } else {
            "failed"
        };
        let mut result = live.value();
        result["status"] = json!(outcome);
        result["exitCode"] = json!(code);
        Ok(result)
    }
    async fn export(&self, args: &[&str], target: &Path, limit: u64) -> Result<()> {
        let temporary = tempfile::Builder::new()
            .prefix(".crow-runtime-")
            .tempfile_in(target.parent().context("Missing export directory")?)?;
        let mut child = Command::new(&self.executable)
            .args(args)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stderr = child.stderr.take().unwrap();
        let captured = std::sync::Mutex::new(Captured::default());
        let mut output = child.stdout.take().unwrap().take(limit + 1);
        let mut file = tokio::fs::File::create(temporary.path()).await?;
        let copy = async {
            let size = tokio::io::copy(&mut output, &mut file).await?;
            ensure!(size <= limit, "Export exceeds {limit} bytes");
            Ok::<_, anyhow::Error>(())
        };
        let (_, _, status) = tokio::try_join!(
            copy,
            async {
                capture_shared(stderr, &captured)
                    .await
                    .map_err(anyhow::Error::from)
            },
            async { child.wait().await.map_err(anyhow::Error::from) }
        )?;
        ensure!(
            status.success(),
            "Could not export container file: {}",
            captured.lock().unwrap().text()
        );
        drop(file);
        temporary.persist(target)?;
        Ok(())
    }
}
const SNAPSHOT_LIMIT: u64 = 512 * 1024 * 1024;
const ARTIFACT_LIMIT: usize = 4 * 1024 * 1024;
fn requested_revision(args: &Value) -> Result<&str> {
    args["revision"]
        .as_str()
        .filter(|s| ["head", "base"].contains(s))
        .context("Revision must be head or base")
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid experiment ID"
    );
    Ok(())
}
fn validate_png(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 33
            && bytes.len() <= ARTIFACT_LIMIT
            && bytes[..8] == *b"\x89PNG\r\n\x1a\n"
            && bytes[12..16] == *b"IHDR",
        "Artifact must be a PNG under 4 MiB"
    );
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    ensure!(
        (1..=4096).contains(&width) && (1..=4096).contains(&height),
        "PNG dimensions exceed 4096 pixels"
    );
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: 64 * 1024 * 1024,
    });
    let mut reader = decoder.read_info().context("Invalid PNG")?;
    let mut decoded = vec![0; reader.output_buffer_size().context("PNG is too large")?];
    reader
        .next_frame(&mut decoded)
        .context("Invalid PNG image data")?;
    Ok(())
}
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

#[derive(Clone, Default)]
struct LiveOutput {
    stdout: std::sync::Arc<std::sync::Mutex<Captured>>,
    stderr: std::sync::Arc<std::sync::Mutex<Captured>>,
}
impl LiveOutput {
    fn value(&self) -> Value {
        let stdout = self.stdout.lock().unwrap();
        let stderr = self.stderr.lock().unwrap();
        json!({"stdout":stdout.text(),"stderr":stderr.text(),"outputTruncated":stdout.truncated||stderr.truncated})
    }
}
async fn capture_shared(
    mut read: impl AsyncRead + Unpin,
    output: &std::sync::Mutex<Captured>,
) -> std::io::Result<()> {
    let mut chunk = [0; 8192];
    loop {
        let count = read.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        };
        output.lock().unwrap().append(&chunk[..count]);
    }
}
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}
impl Captured {
    fn append(&mut self, bytes: &[u8]) {
        const HALF: usize = OUTPUT_LIMIT / 2;
        if !self.truncated && self.bytes.len() + bytes.len() <= OUTPUT_LIMIT {
            self.bytes.extend_from_slice(bytes);
            return;
        }
        // Keep the beginning and the most recent diagnostics, regardless of chunk size.
        let head_missing = HALF.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..head_missing.min(bytes.len())]);
        if bytes.len() >= HALF {
            self.bytes.truncate(HALF);
            self.bytes.extend_from_slice(&bytes[bytes.len() - HALF..]);
        } else {
            let remove = (self.bytes.len() + bytes.len()).saturating_sub(OUTPUT_LIMIT);
            self.bytes.drain(HALF..HALF + remove);
            self.bytes.extend_from_slice(bytes);
        }
        self.truncated = true;
    }
    fn text(&self) -> String {
        if self.truncated {
            format!(
                "{}\n[... output omitted ...]\n{}",
                String::from_utf8_lossy(&self.bytes[..OUTPUT_LIMIT / 2]),
                String::from_utf8_lossy(&self.bytes[OUTPUT_LIMIT / 2..])
            )
        } else {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }
}
#[cfg(test)]
async fn capture(mut read: impl AsyncRead + Unpin, output: &mut Captured) -> std::io::Result<()> {
    let mut chunk = [0; 8192];
    loop {
        let count = read.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        output.append(&chunk[..count]);
    }
    Ok(())
}
async fn podman_output(
    executable: &str,
    args: &[&str],
    env: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(crate::process::run(
        executable,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        crate::process::RunOptions {
            env: Some(env.clone()),
            timeout: Some(Duration::from_secs(20)),
            max_output: 1024 * 1024,
            ..Default::default()
        },
    )
    .await?
    .stdout)
}
async fn check_runtime(executable: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let info: Value =
        serde_json::from_str(&podman_output(executable, &["info", "--format=json"], env).await?)?;
    ensure!(
        info["host"]["security"]["rootless"] == true && info["host"]["serviceIsRemote"] == false,
        "Execution requires local rootless Podman"
    );
    ensure!(
        info["host"]["cgroupVersion"] == "v2" && info["host"]["security"]["seccompEnabled"] == true,
        "Execution requires cgroup v2 and seccomp"
    );
    Ok(())
}
async fn remove_container(
    executable: &str,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    podman_output(executable, &["rm", "--force", "--ignore", name], env).await?;
    Ok(())
}
struct ContainerGuard {
    executable: String,
    name: String,
    env: BTreeMap<String, String>,
    armed: bool,
    started: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if self.armed && self.started.load(std::sync::atomic::Ordering::Relaxed) {
            // Best effort for a dropped future; the runtime deadline also applies
            // if Crow is killed before it can request removal.
            let mut command = std::process::Command::new(&self.executable);
            command
                .args(["rm", "--force", "--ignore", &self.name])
                .env_clear()
                .envs(&self.env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            std::thread::spawn(move || {
                let _ = command.status();
            });
        }
    }
}

/// Append only Crow-recorded outcomes. The model cannot manufacture these rows.
pub fn append_summary(report: &mut Value, dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    crate::report::validate_report(report)?;
    let mut rows = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let record =
            crate::util::read_json(&entry.path())?.context("Missing experiment receipt")?;
        let status = if record["status"] == "running" {
            "interrupted"
        } else {
            record["status"].as_str().unwrap_or("error")
        };
        *counts.entry(status.to_owned()).or_default() += 1;
        let raw = record["command"].as_str().unwrap_or("");
        let mut command: String = raw
            .chars()
            .take(80)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if raw.chars().count() > 80 {
            command.push_str("...");
        }
        command = command
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('|', "&#124;")
            .replace('`', "&#96;");
        let warnings = crate::runtime_diagnostics::warnings(&record);
        let warning_text = if warnings.is_empty() {
            "none".to_owned()
        } else {
            warnings
                .keys()
                .map(|stage| stage.label())
                .collect::<Vec<_>>()
                .join("; ")
        };
        rows.push(format!(
            "| `{}` | {} {} | {} | {} | {} | <code>{}</code> |",
            record["commit"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(12)
                .collect::<String>(),
            record["phase"].as_str().unwrap_or("test"),
            status,
            serde_json::from_value::<Stage>(record["failureStage"].clone())
                .ok()
                .map(Stage::label)
                .unwrap_or("none"),
            warning_text,
            record["exitCode"]
                .as_i64()
                .map_or("none".into(), |c| c.to_string()),
            command
        ));
    }
    if rows.is_empty() {
        if !append_report_suffix(
            report,
            "\n\nRuntime experiments were enabled, but none were run.",
        ) {
            append_report_suffix(report, "\n\nRuntime tests: not attempted.");
        }
        return Ok(());
    }
    rows.sort();
    let total = rows.len();
    rows.truncate(12);
    let counts = counts
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect::<Vec<_>>()
        .join(", ");
    while !rows.is_empty() {
        let rendered = format!(
            "\n\n### Runtime experiments\n\nIsolated setup and offline tests. {counts}. Showing {} of {total} experiments. Outcomes include setup attempts; passing setup does not verify application behavior. Results describe these commands only; failures may reflect environment limits. Full receipts are retained on the worker.\n\n| Commit | Result | Failure location | Warnings | Exit | Command excerpt |\n| --- | --- | --- | --- | --- | --- |\n{}",
            rows.len(),
            rows.join("\n")
        );
        if append_report_suffix(report, &rendered) {
            return Ok(());
        }
        rows.pop();
    }
    // Preserve the model's report even when findings consume the entire publication
    // allowance. Runtime counters also remain in the main status and worker receipts.
    append_report_suffix(
        report,
        &format!("\n\nRuntime setup and test attempts: {counts}."),
    );
    Ok(())
}

fn append_report_suffix(report: &mut Value, suffix: &str) -> bool {
    let mut candidate = report.clone();
    candidate["summary"] = json!(format!(
        "{}{suffix}",
        report["summary"].as_str().unwrap_or("")
    ));
    // Check serialized UTF-8 bytes, summary UTF-16 units and rendered findings
    // together, using the same validation as the final publication path.
    if crate::report::validate_report(&candidate).is_err() {
        return false;
    }
    report["summary"] = candidate["summary"].take();
    true
}

pub async fn diagnostics(settings: &Value) -> Result<Value> {
    let config = config(&settings["execution"])?;
    if !config.automatic && config.repositories.is_empty() {
        return Ok(
            json!({"enabled":false,"detail":"Runtime experiments are disabled. Configure worker.execution to enable selected repositories."}),
        );
    }
    let env = environment();
    check_runtime(&config.podman, &env).await?;
    for policy in config.repositories.values().filter(|p| p.image != "auto") {
        podman_output(&config.podman, &["image", "inspect", &policy.image], &env)
            .await
            .context(
                "Load each configured execution image into the worker user's local Podman store",
            )?;
    }
    Ok(
        json!({"enabled":true,"automatic":config.automatic,"repositories":config.repositories.keys().collect::<Vec<_>>(),"detail":"Local rootless Podman, cgroup v2, seccomp and explicit images are available. Automatic toolchain images are provisioned on first use. Each experiment verifies actual resource limits before running source."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> Value {
        json!({"repositories":{"owner/repo":{"image":format!("sha256:{}", "a".repeat(64))}}})
    }
    #[test]
    fn execution_requires_explicit_repository_and_immutable_image() {
        assert!(!enabled(&json!({}), "owner/repo").unwrap());
        assert!(enabled(&json!({"execution":valid()}), "owner/repo").unwrap());
        assert!(!enabled(&json!({"execution":valid()}), "owner/other").unwrap());
        for image in ["alpine:latest", "--privileged", "sha256:abcd"] {
            let mut cfg = valid();
            cfg["repositories"]["owner/repo"]["image"] = json!(image);
            assert!(validate(&cfg).is_err());
        }
        for (key, value) in [
            ("timeoutSeconds", json!(0)),
            ("memoryMiB", json!(1)),
            ("workspaceMiB", json!(0)),
            ("cpus", json!(0)),
            ("pids", json!(0)),
            ("maxRuns", json!(51)),
            ("network", json!(true)),
            ("privileged", json!(true)),
        ] {
            let mut cfg = valid();
            cfg["repositories"]["owner/repo"][key] = value;
            assert!(validate(&cfg).is_err(), "{key}");
        }
    }
    #[test]
    fn repository_policy_case_matches_enrollment_and_preserves_custom_limits() {
        let root = tempfile::tempdir().unwrap();
        let policy = json!({
            "image":format!("sha256:{}", "b".repeat(64)),
            "timeoutSeconds":75,"memoryMiB":256,"workspaceMiB":128,
            "cpus":1,"pids":32,"maxRuns":4
        });
        for automatic in [false, true] {
            let settings = json!({"execution":{
                "automatic":automatic,"repositories":{"Byntham/Crow":policy}
            }});
            for repo in ["byntham/crow", "Byntham/Crow", "BYNTHAM/CROW"] {
                assert!(enabled(&settings, repo).unwrap());
                let context = json!({"job":{"repo":repo,"settings":settings},"source":{}});
                let execution = Execution::from_context(&context, root.path())
                    .unwrap()
                    .unwrap();
                assert_eq!(execution.repository, "byntham/crow");
                assert_eq!(serde_json::to_value(execution.policy).unwrap(), policy);
            }
            assert_eq!(enabled(&settings, "byntham/other").unwrap(), automatic);
        }
    }

    #[test]
    fn conflicting_repository_spellings_are_rejected_instead_of_selecting_a_policy() {
        let cfg = json!({"automatic":true,"repositories":{
            "Byntham/Crow":{"maxRuns":1},
            "byntham/crow":{"maxRuns":20}
        }});
        let error = validate(&cfg).unwrap_err().to_string();
        assert!(
            error.contains("Duplicate execution repository policy"),
            "{error}"
        );
        assert!(error.contains("byntham/crow"), "{error}");
        assert!(enabled(&json!({"execution":cfg}), "byntham/crow").is_err());
    }

    #[test]
    fn automatic_mode_needs_no_repository_images_or_new_limits() {
        let settings = json!({"execution":{"automatic":true}});
        assert!(enabled(&settings, "any/repository").unwrap());
        let root = tempfile::tempdir().unwrap();
        let context = json!({"job":{"repo":"any/repository","settings":settings},"source":{}});
        let execution = Execution::from_context(&context, root.path())
            .unwrap()
            .unwrap();
        assert_eq!(execution.policy.image, "auto");
        assert_eq!(execution.policy.timeout_seconds, timeout());
        assert_eq!(execution.policy.max_runs, runs());
        assert!(!enabled(&json!({"execution":{"automatic":false}}), "any/repository").unwrap());
        assert!(validate(&json!({"repositories":{"owner/repo":{}}})).is_ok());
    }
    #[test]
    fn invalid_or_oversized_pngs_are_not_sent_to_the_model() {
        assert!(validate_png(b"not an image").is_err());
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[1, 2, 3])
                .unwrap();
        }
        assert!(validate_png(&bytes).is_ok());
        assert!(validate_png(&bytes[..33]).is_err());
        bytes[16..20].copy_from_slice(&10000u32.to_be_bytes());
        assert!(validate_png(&bytes).is_err());
    }
    #[tokio::test]
    async fn budget_and_argument_checks_do_not_start_a_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = valid();
        cfg["podman"] = json!("/no-runtime");
        cfg["repositories"]["owner/repo"]["maxRuns"] = json!(1);
        let context = json!({"job":{"repo":"owner/repo","settings":{"execution":cfg}},"source":{"head":"a".repeat(40),"base":"b".repeat(40)}});
        let execution = Execution::from_context(&context, dir.path())
            .unwrap()
            .unwrap();
        for args in [
            json!({"revision":"HEAD","command":"true"}),
            json!({"revision":"head","command":""}),
            json!({"revision":"head","command":"true","image":"other"}),
        ] {
            assert!(
                execution
                    .call("run_experiment", &args, CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        crate::util::atomic(&dir.path().join("done.json"), &json!({"status":"passed"})).unwrap();
        assert!(
            execution
                .call(
                    "run_experiment",
                    &json!({"revision":"head","command":"true"}),
                    CancellationToken::new()
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        let saved = execution
            .call("list_experiments", &json!({}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(saved["runs"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn large_experiment_history_stays_publishable_without_hiding_counts() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..50 {
            crate::util::atomic(&dir.path().join(format!("{n}.json")), &json!({"commit":"a".repeat(40),"status":"failed","exitCode":1,"command":"|".repeat(16000)})).unwrap();
        }
        let mut report = json!({"summary":"x".repeat(8000),"findings":[]});
        append_summary(&mut report, dir.path()).unwrap();
        let report = crate::report::validate_report(&report).unwrap();
        let summary = report["summary"].as_str().unwrap();
        assert!(summary.contains("50 failed") && summary.contains("Showing 12 of 50"));
        assert!(summary.contains("&#124;"));
    }

    #[test]
    fn receipt_warnings_are_visible_bounded_and_exclude_private_errors() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..50 {
            crate::util::atomic(&dir.path().join(format!("{n}.json")), &json!({
                "commit":"a".repeat(40), "phase":"setup", "status":"passed", "exitCode":0,
                "command":"|".repeat(16000), "cleanupError":"private cleanup detail",
                "cacheRestoreError":"private cache detail", "cacheSaveError":"private cache detail",
                "artifacts":[{"saved":false,"error":"private artifact detail"}]
            })).unwrap();
        }
        let mut report = json!({"summary":"x".repeat(8000),"findings":[]});
        append_summary(&mut report, dir.path()).unwrap();
        crate::report::validate_report(&report).unwrap();
        let summary = report["summary"].as_str().unwrap();
        assert!(summary.contains("50 passed"));
        assert!(summary.contains("dependency cache restore"));
        assert!(summary.contains("dependency cache save"));
        assert!(summary.contains("screenshot collection"));
        assert!(summary.contains("container cleanup"));
        assert!(!summary.contains("private"));
    }

    fn unicode_report_with_serialized_size(bytes: usize) -> Value {
        let findings = (0..10)
            .map(|index| {
                json!({
                    "title":format!("Finding {index}"), "body":"🦀".repeat(900),
                    "path":format!("src/file{index}.rs"), "line":1, "severity":"medium"
                })
            })
            .collect::<Vec<_>>();
        let mut report = crate::report::validate_report(
            &json!({"summary":"Review summary.","findings":findings}),
        )
        .unwrap();
        let padding = bytes
            .checked_sub(serde_json::to_vec(&report).unwrap().len())
            .unwrap();
        report["summary"] = json!(format!("Review summary.{}", "s".repeat(padding)));
        let report = crate::report::validate_report(&report).unwrap();
        assert_eq!(serde_json::to_vec(&report).unwrap().len(), bytes);
        report
    }

    #[test]
    fn receipt_appendix_respects_whole_report_bytes_and_preserves_unicode_findings() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..12 {
            crate::util::atomic(
                &dir.path().join(format!("{index}.json")),
                &json!({
                    "commit":"a".repeat(40), "phase":"test", "status":"failed", "exitCode":1,
                    "command":"🦀\\\"".repeat(80)
                }),
            )
            .unwrap();
        }
        for bytes in [43_000, 44_750, 45_000] {
            let original = unicode_report_with_serialized_size(bytes);
            let mut report = original.clone();
            append_summary(&mut report, dir.path()).unwrap();
            let checked = crate::report::validate_report(&report).unwrap();
            assert_eq!(checked["findings"], original["findings"]);
            let summary = checked["summary"].as_str().unwrap();
            assert!(summary.starts_with(original["summary"].as_str().unwrap()));
            if bytes == 45_000 {
                assert_eq!(checked, original);
            } else {
                assert!(summary.contains("12 failed"));
                if bytes == 44_750 {
                    assert!(summary.contains("Runtime setup and test attempts"));
                    assert!(!summary.contains("| Commit |"));
                }
            }
        }
    }

    #[test]
    fn no_attempt_note_cannot_overflow_a_valid_report() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [44_960, 45_000] {
            let original = unicode_report_with_serialized_size(bytes);
            let mut report = original.clone();
            append_summary(&mut report, dir.path()).unwrap();
            let checked = crate::report::validate_report(&report).unwrap();
            assert_eq!(checked["findings"], original["findings"]);
            if bytes == 45_000 {
                assert_eq!(checked, original);
            } else {
                assert!(
                    checked["summary"]
                        .as_str()
                        .unwrap()
                        .ends_with("Runtime tests: not attempted.")
                );
            }
        }
    }

    #[test]
    fn full_utf16_summary_is_preserved_with_or_without_runtime_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let original =
            crate::report::validate_report(&json!({"summary":"🦀".repeat(8000),"findings":[]}))
                .unwrap();
        let mut report = original.clone();
        append_summary(&mut report, dir.path()).unwrap();
        assert_eq!(report, original);
        crate::util::atomic(
            &dir.path().join("one.json"),
            &json!({"status":"passed","phase":"test","command":"true"}),
        )
        .unwrap();
        append_summary(&mut report, dir.path()).unwrap();
        assert_eq!(report, original);
        crate::report::validate_report(&report).unwrap();
    }

    #[test]
    fn output_capture_keeps_head_and_tail_across_chunk_boundaries() {
        for chunk_size in [1, 17, 8192, OUTPUT_LIMIT, OUTPUT_LIMIT * 3] {
            let bytes = [
                b"FIRST_DIAGNOSTIC\n".as_slice(),
                &vec![b'x'; OUTPUT_LIMIT * 2],
                b"\nFINAL_FAILURE_DIAGNOSIS",
            ]
            .concat();
            let mut captured = Captured::default();
            for chunk in bytes.chunks(chunk_size) {
                captured.append(chunk);
            }
            assert_eq!(captured.bytes.len(), OUTPUT_LIMIT);
            let text = captured.text();
            assert!(text.starts_with("FIRST_DIAGNOSTIC"));
            assert!(text.contains("[... output omitted ...]"));
            assert!(text.ends_with("FINAL_FAILURE_DIAGNOSIS"));
        }
        let mut captured = Captured::default();
        captured.append("small UTF-8: café".as_bytes());
        assert_eq!(captured.text(), "small UTF-8: café");
        assert!(!captured.truncated);
    }

    #[tokio::test]
    async fn output_capture_drains_both_bounded_and_invalid_utf8_output() {
        let bytes = vec![0xff; OUTPUT_LIMIT + 100];
        let mut output = Captured::default();
        capture(bytes.as_slice(), &mut output).await.unwrap();
        assert_eq!(output.bytes.len(), OUTPUT_LIMIT);
        assert!(output.truncated);
    }
}
