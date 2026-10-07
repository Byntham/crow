//! One experiment container: rootless Podman, a non-root user that maps to a
//! subordinate host UID, no capabilities, no new namespaces, a read-only root
//! filesystem, verified cgroup limits, and no network unless it is a setup
//! container using the gateway.
use super::{Limits, gateway::Gateway, seccomp};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

/// Container user. Rootless Podman maps it to a subordinate UID, so code in the
/// container never runs as the operator's own account.
const USER: &str = "1000:1000";
const OUTPUT_LIMIT: usize = 32 * 1024;
/// Time allowed for starting the container, restoring files and exporting a snapshot.
const OVERHEAD_SECONDS: u64 = 120;
/// Shell prelude for setup: start the bridge to the package gateway and route
/// package managers through it. Tests never receive these variables.
const SETUP_PRELUDE: &str = r#"python3 -I /opt/crow/proxy.py >/tmp/crow-proxy.log 2>&1 &
crow_proxy_pid=$!
trap 'kill $crow_proxy_pid 2>/dev/null || true' EXIT
proxy=http://127.0.0.1:3128
export HTTPS_PROXY=$proxy HTTP_PROXY=$proxy https_proxy=$proxy http_proxy=$proxy NO_PROXY=127.0.0.1,localhost
export YARN_HTTP_PROXY=$proxy YARN_HTTPS_PROXY=$proxy
python3 -I -c 'import socket,time
for _ in range(100):
    try:
        socket.create_connection(("127.0.0.1", 3128), timeout=.1).close(); break
    except OSError:
        time.sleep(.05)
else:
    raise SystemExit("Crow package gateway bridge did not start")' || { cat /tmp/crow-proxy.log >&2; exit 125; }
# Crow setup command
"#;

/// One container run.
pub(super) struct Run<'a> {
    pub name: String,
    pub review: &'a str,
    pub image: &'a str,
    /// Private directory for the seccomp profile and the gateway socket.
    pub scratch: &'a Path,
    /// Tar archive unpacked into /workspace first.
    pub workspace: &'a Path,
    /// Pinned source restored over a prepared workspace before a test.
    pub restore: Option<&'a Path>,
    pub command: &'a str,
    /// Where a successful setup saves the workspace. `None` for tests.
    pub snapshot: Option<&'a Path>,
}

#[derive(Debug, Default)]
pub(super) struct Outcome {
    /// `passed` or `failed` for the command, `error` when the sandbox itself failed.
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub error: Option<String>,
    pub gateway_errors: Vec<String>,
}
impl Outcome {
    fn sandbox_error(message: impl Into<String>) -> Self {
        Self {
            status: "error",
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

pub(super) struct Sandbox<'a> {
    pub podman: &'a str,
    pub env: &'a BTreeMap<String, String>,
    pub limits: &'a Limits,
}
impl Sandbox<'_> {
    /// Run one experiment. Sandbox failures become an `error` outcome rather
    /// than an `Err`, so the receipt records them.
    pub async fn run(&self, run: Run<'_>, cancel: &CancellationToken) -> Outcome {
        let profile = run.scratch.join("seccomp.profile");
        let written = seccomp::profile(self.podman, self.env)
            .await
            .and_then(|p| crate::util::atomic(&profile, &p));
        if let Err(e) = written {
            return Outcome::sandbox_error(format!("Seccomp profile: {e:#}"));
        }
        let gateway = if run.snapshot.is_some() {
            match Gateway::start_in(run.scratch, self.limits) {
                Ok(gateway) => Some(gateway),
                Err(e) => return Outcome::sandbox_error(format!("Package gateway: {e:#}")),
            }
        } else {
            None
        };
        let args = container_args(
            self.limits,
            &run.name,
            run.review,
            run.image,
            &profile,
            gateway.as_ref().map(Gateway::directory),
        );
        let guard = Container {
            podman: self.podman.to_owned(),
            env: self.env.clone(),
            name: run.name.clone(),
            armed: true,
        };
        let mut outcome = tokio::select! {
            outcome = self.steps(&run, &args, gateway.is_some()) => outcome,
            _ = cancel.cancelled() => Outcome::sandbox_error("Interrupted"),
        };
        if let Some(gateway) = gateway {
            outcome.gateway_errors = gateway.close().await;
        }
        guard.remove().await;
        outcome
    }
    async fn steps(&self, run: &Run<'_>, args: &[String], setup: bool) -> Outcome {
        match self.podman(args, None).await {
            Ok(started) if started.success => {}
            Ok(started) => {
                return Outcome::sandbox_error(format!(
                    "Container start: {}",
                    started.stderr.trim()
                ));
            }
            Err(e) => return Outcome::sandbox_error(format!("Container start: {e:#}")),
        }
        // A prepared workspace is archived as `workspace/...`. Stripping that top
        // entry leaves the mount itself alone, which the container user cannot change.
        let strip = if run.restore.is_some() {
            " --strip-components=1"
        } else {
            ""
        };
        let check = format!(
            "{}\n{}\ntar -xf - --delay-directory-restore{strip} -C /workspace && mkdir -p /workspace/.home",
            limits_check(self.limits),
            seccomp::CHECK
        );
        let exec = |script: &str| -> Vec<String> {
            ["exec", "--interactive", &run.name, "/bin/sh", "-c", script]
                .map(str::to_owned)
                .to_vec()
        };
        match self.podman(&exec(&check), Some(run.workspace)).await {
            Ok(unpacked) if unpacked.success => {}
            Ok(unpacked) => {
                return Outcome::sandbox_error(format!(
                    "Could not verify the sandbox or unpack the workspace: {}",
                    unpacked.stderr.trim()
                ));
            }
            Err(e) => return Outcome::sandbox_error(format!("Workspace: {e:#}")),
        }
        if let Some(restore) = run.restore {
            // Setup may edit tracked files; always test the pinned source.
            let args: Vec<String> = [
                "exec",
                "--interactive",
                &run.name,
                "python3",
                "-I",
                "-c",
                include_str!("restore.py"),
            ]
            .map(str::to_owned)
            .to_vec();
            match self.podman(&args, Some(restore)).await {
                Ok(restored) if restored.success => {}
                Ok(restored) => {
                    return Outcome::sandbox_error(format!(
                        "Could not restore the pinned source: {}",
                        restored.stderr.trim()
                    ));
                }
                Err(e) => return Outcome::sandbox_error(format!("Source restore: {e:#}")),
            }
        }
        let script = if setup {
            format!("{SETUP_PRELUDE}{}", run.command)
        } else {
            run.command.to_owned()
        };
        let command: Vec<String> = ["exec", &run.name, "/bin/sh", "-c", &script]
            .map(str::to_owned)
            .to_vec();
        let limit = Duration::from_secs(self.limits.timeout_seconds);
        let executed = match tokio::time::timeout(limit, self.podman(&command, None)).await {
            Ok(Ok(executed)) => executed,
            Ok(Err(e)) => return Outcome::sandbox_error(format!("Command: {e:#}")),
            Err(_) => {
                return Outcome {
                    status: "failed",
                    error: Some(format!(
                        "Timed out after {} seconds",
                        self.limits.timeout_seconds
                    )),
                    ..Default::default()
                };
            }
        };
        let mut outcome = Outcome {
            // Exit codes 125-127 mean the shell or Podman could not run the command.
            status: match executed.code {
                Some(0) => "passed",
                Some(125..=127) | None => "error",
                Some(_) => "failed",
            },
            exit_code: executed.code,
            stdout: executed.stdout,
            stderr: executed.stderr,
            truncated: executed.truncated,
            ..Default::default()
        };
        if let Some(snapshot) = run.snapshot
            && outcome.status == "passed"
        {
            // Owner-writable entries let tar recreate read-only module caches.
            // Download caches are not needed once dependencies are installed.
            let export: Vec<String> = [
                "exec",
                &run.name,
                "tar",
                "--mode=u+rwX",
                "--exclude=workspace/.home/.npm",
                "--exclude=workspace/.home/.cache",
                "-cf",
                "-",
                "-C",
                "/",
                "workspace",
            ]
            .map(str::to_owned)
            .to_vec();
            if let Err(e) = self.export(&export, snapshot).await {
                outcome.status = "error";
                outcome.error = Some(format!("Could not save the prepared workspace: {e:#}"));
            }
        }
        outcome
    }
    async fn podman(&self, args: &[String], input: Option<&Path>) -> Result<Executed> {
        execute(self.podman, self.env, args, input).await
    }
    /// Stream a container file into `target`, bounded by the workspace size.
    async fn export(&self, args: &[String], target: &Path) -> Result<()> {
        let partial = target.with_extension("partial");
        let result = self.stream(args, &partial).await.and_then(|()| {
            std::fs::rename(&partial, target).context("Could not store the snapshot")
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    }
    async fn stream(&self, args: &[String], partial: &Path) -> Result<()> {
        let limit = self.limits.workspace_mib * 1024 * 1024;
        let mut child = Command::new(self.podman)
            .args(args)
            .env_clear()
            .envs(self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = child.stdout.take().unwrap().take(limit + 1);
        let stderr = child.stderr.take().unwrap();
        let errors = Mutex::new(Captured::default());
        let mut file = tokio::fs::File::create(partial).await?;
        let copy = async {
            let copied = tokio::io::copy(&mut stdout, &mut file).await;
            // Past the limit nothing reads the archive, so tar would block until
            // the container's timeout. Stop the export instead.
            if !matches!(copied, Ok(n) if n <= limit) {
                let _ = child.start_kill();
            }
            (copied, child.wait().await)
        };
        let ((copied, status), captured) = tokio::join!(copy, capture(stderr, &errors));
        let (copied, status) = (copied?, status?);
        captured?;
        ensure!(
            copied <= limit,
            "the workspace exceeds {} MiB",
            self.limits.workspace_mib
        );
        ensure!(status.success(), "{}", errors.lock().unwrap().text());
        Ok(())
    }
}

/// Arguments for `podman run`. Kept pure so tests can check every isolation flag.
pub(super) fn container_args(
    limits: &Limits,
    name: &str,
    review: &str,
    image: &str,
    seccomp: &Path,
    gateway: Option<&Path>,
) -> Vec<String> {
    let lifetime = limits.timeout_seconds + OVERHEAD_SECONDS;
    let scratch = limits.workspace_mib;
    let mut args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--rm".into(),
        "--pull=never".into(),
        format!("--name={name}"),
        format!("--label=crow.review={review}"),
        "--network=none".into(),
        "--read-only".into(),
        "--read-only-tmpfs=false".into(),
        "--image-volume=ignore".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        format!("--security-opt=seccomp={}", seccomp.display()),
        format!("--user={USER}"),
        "--pid=private".into(),
        "--ipc=private".into(),
        "--log-driver=none".into(),
        "--http-proxy=false".into(),
        "--env=HOME=/workspace/.home".into(),
        "--env=CI=true".into(),
        format!("--memory={}m", limits.memory_mib),
        format!("--memory-swap={}m", limits.memory_mib),
        format!("--cpus={}", limits.cpus),
        format!("--pids-limit={}", limits.pids),
        // Podman stops the container even if Crow exits; --rm then removes it.
        format!("--timeout={lifetime}"),
        "--stop-timeout=1".into(),
        "--ulimit=nofile=1024:1024".into(),
        "--ulimit=core=0:0".into(),
        format!("--tmpfs=/workspace:rw,exec,nosuid,nodev,size={scratch}m,mode=1777"),
        format!(
            "--tmpfs=/tmp:rw,exec,nosuid,nodev,size={}m,mode=1777",
            scratch.min(256)
        ),
        "--workdir=/workspace".into(),
    ];
    if let Some(gateway) = gateway {
        args.push(format!(
            "--volume={}:/run/crow-downloads:ro,Z",
            gateway.display()
        ));
    }
    args.extend([
        "--entrypoint=/bin/sh".into(),
        image.into(),
        "-c".into(),
        format!("exec sleep {lifetime}"),
    ]);
    args
}

/// Shell check that runs before any repository file is unpacked: the user is not
/// root and the cgroup limits match the requested ones.
fn limits_check(limits: &Limits) -> String {
    format!(
        "[ \"$(id -u)\" != 0 ] \
         && read memory < /sys/fs/cgroup/memory.max && [ \"$memory\" = {memory} ] \
         && read pids < /sys/fs/cgroup/pids.max && [ \"$pids\" = {pids} ] \
         && read quota period < /sys/fs/cgroup/cpu.max && [ \"$quota\" != max ] \
         && [ \"$quota\" -le $(({cpus} * period)) ] \
         || {{ echo 'Crow could not verify the container user or resource limits' >&2; exit 125; }}",
        memory = limits.memory_mib * 1024 * 1024,
        pids = limits.pids,
        cpus = limits.cpus,
    )
}

/// Removes the container when the run ends, including when it is cancelled.
struct Container {
    podman: String,
    env: BTreeMap<String, String>,
    name: String,
    armed: bool,
}
impl Container {
    async fn remove(mut self) {
        let args = ["rm", "--force", "--time=0", "--ignore", &self.name].map(str::to_owned);
        let _ = execute(&self.podman, &self.env, &args, None).await;
        self.armed = false;
    }
}
impl Drop for Container {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A dropped run (for example a cancelled tool call) still removes its container.
        let _ = std::process::Command::new(&self.podman)
            .args(["rm", "--force", "--time=0", "--ignore", &self.name])
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

pub(super) struct Executed {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}
pub(super) async fn execute(
    program: &str,
    env: &BTreeMap<String, String>,
    args: &[String],
    input: Option<&Path>,
) -> Result<Executed> {
    let stdin = match input {
        Some(path) => Stdio::from(std::fs::File::open(path)?),
        None => Stdio::null(),
    };
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Cannot start {program}"))?;
    let stdout = Arc::new(Mutex::new(Captured::default()));
    let stderr = Arc::new(Mutex::new(Captured::default()));
    let (_, _, status) = tokio::try_join!(
        capture(child.stdout.take().unwrap(), &stdout),
        capture(child.stderr.take().unwrap(), &stderr),
        child.wait()
    )?;
    let (stdout, stderr) = (stdout.lock().unwrap(), stderr.lock().unwrap());
    Ok(Executed {
        success: status.success(),
        code: status.code(),
        stdout: stdout.text(),
        stderr: stderr.text(),
        truncated: stdout.truncated || stderr.truncated,
    })
}
async fn capture(
    mut read: impl AsyncRead + Unpin,
    output: &Mutex<Captured>,
) -> std::io::Result<()> {
    let mut chunk = [0; 8192];
    loop {
        let count = read.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        output.lock().unwrap().append(&chunk[..count]);
    }
}

/// Command output bounded to its beginning and end, where diagnostics usually are.
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

/// Remove this review's leftover containers, for example after a crash.
pub(super) async fn remove_review_containers(
    podman: &str,
    env: &BTreeMap<String, String>,
    review: &str,
) -> Result<()> {
    let filter = format!("--filter=label=crow.review={review}");
    let listed = execute(
        podman,
        env,
        &["ps", "--all", "--quiet", &filter].map(str::to_owned),
        None,
    )
    .await?;
    if !listed.success {
        bail!(
            "Could not list runtime containers: {}",
            listed.stderr.trim()
        );
    }
    for id in listed.stdout.split_whitespace() {
        let removed = execute(
            podman,
            env,
            &["rm", "--force", "--time=0", "--ignore", id].map(str::to_owned),
            None,
        )
        .await?;
        if !removed.success {
            bail!(
                "Could not remove runtime container {id}: {}",
                removed.stderr.trim()
            );
        }
    }
    Ok(())
}

pub(super) fn outcome_json(outcome: &Outcome) -> Value {
    let mut value = json!({
        "status": outcome.status,
        "exitCode": outcome.exit_code,
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "outputTruncated": outcome.truncated,
    });
    if let Some(error) = &outcome.error {
        value["error"] = json!(error);
    }
    if !outcome.gateway_errors.is_empty() {
        value["gatewayErrors"] = json!(outcome.gateway_errors);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_isolated_and_bounded() {
        let limits = Limits::default();
        let profile = Path::new("/s/seccomp.profile");
        let args = container_args(&limits, "crow-x-1", "review1", "image:tag", profile, None);
        for flag in [
            "--rm",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--security-opt=seccomp=/s/seccomp.profile",
            "--user=1000:1000",
            "--pull=never",
            "--image-volume=ignore",
            "--http-proxy=false",
            "--label=crow.review=review1",
        ] {
            assert!(args.contains(&flag.to_owned()), "missing {flag}");
        }
        assert!(!args.iter().any(|a| a.starts_with("--volume")));
        assert!(args.contains(&format!("--memory={}m", limits.memory_mib)));
        assert!(args.contains(&format!("--memory-swap={}m", limits.memory_mib)));
        assert!(args.contains(&format!("--pids-limit={}", limits.pids)));
        let setup = container_args(
            &limits,
            "crow-x-2",
            "review1",
            "image:tag",
            profile,
            Some(Path::new("/g")),
        );
        assert!(setup.contains(&"--volume=/g:/run/crow-downloads:ro,Z".to_owned()));
        // The image and its command come last, after every option.
        assert_eq!(setup[setup.len() - 3], "image:tag");
    }
    #[test]
    fn limits_check_requires_non_root_and_exact_limits() {
        let check = limits_check(&Limits::default());
        assert!(check.contains("id -u"));
        assert!(check.contains(&format!("= {} ]", Limits::default().pids)));
        assert!(check.contains("exit 125"));
    }
    #[tokio::test]
    async fn oversized_export_stops_without_waiting_for_the_container() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::env::vars().filter(|(k, _)| k == "PATH").collect();
        let limits = Limits {
            workspace_mib: 1,
            ..Limits::default()
        };
        let sandbox = Sandbox {
            podman: "/bin/sh",
            env: &env,
            limits: &limits,
        };
        // `yes` never ends on its own, like tar blocked on a full pipe.
        let target = dir.path().join("snapshot.tar");
        let args = ["-c", "exec yes"].map(str::to_owned);
        let exported =
            tokio::time::timeout(Duration::from_secs(10), sandbox.export(&args, &target))
                .await
                .expect("the export must stop at the limit");
        assert!(exported.unwrap_err().to_string().contains("exceeds 1 MiB"));
        assert!(!target.exists() && !target.with_extension("partial").exists());
    }
    #[test]
    fn captured_output_keeps_beginning_and_end() {
        let mut output = Captured::default();
        output.append(b"start\n");
        output.append(&vec![b'x'; OUTPUT_LIMIT * 2]);
        output.append(b"\nfinal error");
        let text = output.text();
        assert!(output.truncated);
        assert!(text.starts_with("start"));
        assert!(text.ends_with("final error"));
        assert!(text.len() < OUTPUT_LIMIT + 64);
    }
}
