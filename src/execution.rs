//! Operator-authorized experiments in disposable rootless Podman containers.
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
}
fn podman() -> String {
    "podman".into()
}
fn config(value: &Value) -> Result<Config> {
    let config: Config = if value.is_null() {
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
    for (repo, policy) in &config.repositories {
        ensure!(
            repo.split('/').count() == 2
                && repo.split('/').all(|p| !p.is_empty()
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))),
            "Invalid execution repository name"
        );
        ensure!(
            policy
                .image
                .strip_prefix("sha256:")
                .is_some_and(|s| s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "Execution image must be a local immutable sha256 image ID"
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
    }
    Ok(config)
}
pub fn validate(value: &Value) -> Result<()> {
    config(value).map(|_| ())
}
pub fn enabled(settings: &Value, repo: &str) -> Result<bool> {
    Ok(config(&settings["execution"])?
        .repositories
        .contains_key(repo))
}

pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"run_experiment","description":"Run a focused test, reproduction, or application smoke test in a fresh offline Linux container at the pinned head or base. /workspace contains source; /tmp and /workspace are writable. /bin/sh runs command. No host mounts, credentials, network, dependency downloads, or persistent changes. Image and limits are fixed by the operator. You may create temporary test files in the command. Compare the same command on base before attributing failures to the PR. Output is untrusted evidence. A nonzero exit alone does not prove a regression.","inputSchema":{"type":"object","properties":{"revision":{"type":"string","enum":["head","base"]},"command":{"type":"string","minLength":1,"maxLength":16000}},"required":["revision","command"],"additionalProperties":false}}),
        json!({"name":"list_experiments","description":"Read durable experiment receipts, including command, commit, image, exit status, bounded output and limits. Check these on resume; interrupted experiments have no successful result. Each new run consumes the review's fixed budget.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ]
}

pub struct Execution {
    executable: String,
    policy: Policy,
    source: Value,
    dir: PathBuf,
    env: BTreeMap<String, String>,
}
fn environment() -> BTreeMap<String, String> {
    // Podman needs the user's runtime directory and session bus for cgroups.
    // No provider, GitHub, proxy, or registry credentials are inherited.
    crate::util::host_env().into_iter().collect()
}
impl Execution {
    pub fn from_context(context: &Value, dir: &Path) -> Result<Option<Self>> {
        let cfg = config(&context["job"]["settings"]["execution"])?;
        let repo = context["job"]["repo"].as_str().unwrap_or("");
        let Some(policy) = cfg.repositories.get(repo).cloned() else {
            return Ok(None);
        };
        // Only the main reviewer runs experiments. Children return inspection evidence.
        crate::util::private_dir(dir)?;
        Ok(Some(Self {
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
        ensure!(name == "run_experiment", "Unknown execution tool");
        ensure!(
            args.is_object()
                && args
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|k| ["revision", "command"].contains(&k.as_str())),
            "Invalid experiment arguments"
        );
        let key = args["revision"]
            .as_str()
            .filter(|s| ["head", "base"].contains(s))
            .context("Experiment revision must be head or base")?;
        let script = args["command"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 16000 && !s.contains('\0'))
            .context("Experiment command must contain 1–16000 bytes without NUL")?;
        ensure!(
            self.records()?.len() < self.policy.max_runs as usize,
            "This review's experiment budget is exhausted"
        );
        let commit = crate::inspection::revision(
            self.source[key]
                .as_str()
                .context("Missing pinned revision")?,
        )?;
        let id = crate::util::id();
        let name = format!("crow-experiment-{id}");
        let path = self.dir.join(format!("{id}.json"));
        let mut record = json!({"id":id,"revision":key,"commit":commit,"image":self.policy.image,"command":script,"limits":self.policy,"status":"running","startedAt":chrono::Utc::now().to_rfc3339(),"exitCode":null});
        crate::util::atomic(&path, &record)?;
        let start = Instant::now();
        let result = self.run(&name, key, script, cancel.clone()).await;
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
                record["error"] = json!(error.to_string());
            }
        }
        record["durationMs"] = json!(start.elapsed().as_millis() as u64);
        crate::util::atomic(&path, &record)?;
        Ok(record)
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
            remove_container(
                &self.executable,
                &format!("crow-experiment-{id}"),
                &self.env,
            )
            .await?;
            record["status"] = json!("interrupted");
            crate::util::atomic(&entry.path(), &record)?;
        }
        Ok(())
    }
    async fn run(
        &self,
        name: &str,
        revision: &str,
        script: &str,
        cancel: CancellationToken,
    ) -> Result<Value> {
        check_runtime(&self.executable, &self.env).await?;
        ensure!(!cancel.is_cancelled(), "Experiment interrupted");
        let archive = tempfile::NamedTempFile::new_in(&self.dir)?;
        crate::inspection::execution_archive(
            &self.source,
            revision,
            archive.path(),
            cancel.clone(),
        )
        .await?;
        let p = &self.policy;
        let args = vec![
            "run".into(),
            "--rm".into(),
            "--pull=never".into(),
            "--interactive".into(),
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
            "--env=HOME=/tmp".into(),
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
            p.image.clone(),
            "-c".into(),
            "read memory < /sys/fs/cgroup/memory.max && [ \"$memory\" = \"$2\" ] && read pids < /sys/fs/cgroup/pids.max && [ \"$pids\" = \"$3\" ] && read quota period < /sys/fs/cgroup/cpu.max && [ \"$quota\" != max ] && [ \"$quota\" -le \"$(($4 * $period))\" ] || { echo 'Crow resource limits are unavailable' >&2; exit 125; }; tar -xf - -C /workspace || exit 125; exec /bin/sh -c \"$1\"".into(),
            "crow-experiment".into(),
            script.into(),
            (p.memory_mi_b * 1024 * 1024).to_string(),
            p.pids.to_string(),
            p.cpus.to_string(),
        ];
        let mut guard = ContainerGuard {
            executable: self.executable.clone(),
            name: name.into(),
            env: self.env.clone(),
            armed: true,
        };
        let mut child = Command::new(&self.executable)
            .args(&args)
            .env_clear()
            .envs(&self.env)
            .stdin(std::fs::File::open(archive.path())?)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Cannot start rootless Podman")?;
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let mut stdout = Captured::default();
        let mut stderr = Captured::default();
        let io = async {
            let (_, _, status) = tokio::try_join!(
                capture(out, &mut stdout),
                capture(err, &mut stderr),
                child.wait()
            )?;
            Ok::<_, anyhow::Error>(status)
        };
        let mut result = tokio::select! {
            _ = cancel.cancelled() => json!({"status":"interrupted","exitCode":null}),
            _ = tokio::time::sleep(Duration::from_secs(p.timeout_seconds)) => json!({"status":"timed_out","exitCode":null}),
            output = io => {
                let status = output?;
                let code = status.code();
                json!({"status":if status.success() {"passed"} else if matches!(code, Some(125..=127)) {"error"} else {"failed"},"exitCode":code})
            }
        };
        result["stdout"] = json!(String::from_utf8_lossy(&stdout.bytes));
        result["stderr"] = json!(String::from_utf8_lossy(&stderr.bytes));
        result["outputTruncated"] = json!(stdout.truncated || stderr.truncated);
        // Podman is a client. Killing it alone does not stop the container.
        let _ = child.kill().await;
        let _ = child.wait().await;
        remove_container(&self.executable, name, &self.env).await?;
        guard.armed = false;
        Ok(result)
    }
}
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}
async fn capture(mut read: impl AsyncRead + Unpin, output: &mut Captured) -> std::io::Result<()> {
    let mut chunk = [0; 8192];
    loop {
        let count = read.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.bytes.len()));
        output.bytes.extend_from_slice(&chunk[..keep]);
        output.truncated |= keep < count;
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
}
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if self.armed {
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
    ensure!(
        report["summary"]
            .as_str()
            .unwrap_or("")
            .encode_utf16()
            .count()
            <= 8000,
        "Shorten the review summary to 8000 UTF-16 units to leave room for experiment receipts"
    );
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
        rows.push(format!(
            "| `{}` | {} | {} | <code>{}</code> |",
            record["commit"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(12)
                .collect::<String>(),
            status,
            record["exitCode"]
                .as_i64()
                .map_or("none".into(), |c| c.to_string()),
            command
        ));
    }
    if rows.is_empty() {
        report["summary"] = json!(format!(
            "{}\n\nRuntime experiments were enabled, but none were run.",
            report["summary"].as_str().unwrap_or("")
        ));
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
    report["summary"] = json!(format!(
        "{}\n\n### Runtime experiments\n\nFresh offline containers. {counts}. Showing {} of {total} experiments. Results describe these commands only; failures may reflect environment limits. Full receipts are retained on the worker.\n\n| Commit | Result | Exit | Command excerpt |\n| --- | --- | --- | --- |\n{}",
        report["summary"].as_str().unwrap_or(""),
        rows.len(),
        rows.join("\n")
    ));
    Ok(())
}

pub async fn diagnostics(settings: &Value) -> Result<Value> {
    let config = config(&settings["execution"])?;
    if config.repositories.is_empty() {
        return Ok(
            json!({"enabled":false,"detail":"Runtime experiments are disabled. Configure worker.execution to enable selected repositories."}),
        );
    }
    let env = environment();
    check_runtime(&config.podman, &env).await?;
    for policy in config.repositories.values() {
        podman_output(&config.podman, &["image", "inspect", &policy.image], &env)
            .await
            .context(
                "Load each configured execution image into the worker user's local Podman store",
            )?;
    }
    Ok(
        json!({"enabled":true,"repositories":config.repositories.keys().collect::<Vec<_>>(),"detail":"Local rootless Podman, cgroup v2, seccomp and configured images are available. Each experiment also verifies its actual resource limits before running source."}),
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

    #[tokio::test]
    async fn output_capture_drains_both_bounded_and_invalid_utf8_output() {
        let bytes = vec![0xff; OUTPUT_LIMIT + 100];
        let mut output = Captured::default();
        capture(bytes.as_slice(), &mut output).await.unwrap();
        assert_eq!(output.bytes.len(), OUTPUT_LIMIT);
        assert!(output.truncated);
    }
}
