//! Bounded subprocess execution with cancellation and process-group cleanup.
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, process::Stdio, sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

pub type LineCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;
pub struct RunOptions {
    pub cwd: Option<PathBuf>,
    pub env: Option<BTreeMap<String, String>>,
    pub input: Option<String>,
    pub cancel: CancellationToken,
    pub timeout: Option<Duration>,
    pub max_output: usize,
    pub detached: bool,
    pub capture: bool,
    pub inherit: bool,
    pub on_line: Option<LineCallback>,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            input: None,
            cancel: CancellationToken::new(),
            timeout: None,
            max_output: 16 * 1024 * 1024,
            detached: true,
            capture: true,
            inherit: false,
            on_line: None,
        }
    }
}
#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}
struct Group {
    pid: u32,
    detached: bool,
}
impl Group {
    fn signal(&self, signal: i32) {
        #[cfg(unix)]
        unsafe {
            libc::kill(
                if self.detached {
                    -(self.pid as i32)
                } else {
                    self.pid as i32
                },
                signal,
            );
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        if self.detached {
            self.signal(libc::SIGKILL);
        }
    }
}

pub async fn run(program: &str, args: &[String], options: RunOptions) -> Result<Output> {
    if options.cancel.is_cancelled() {
        bail!("Interrupted");
    }
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    command.env_clear();
    if let Some(env) = &options.env {
        command.envs(env);
    } else {
        command.envs(crate::util::host_env());
    }
    #[cfg(unix)]
    if options.detached {
        command.process_group(0);
    }
    if options.inherit {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start {program}"))?;
    let group = Group {
        pid: child.id().context("Process has no ID")?,
        detached: options.detached,
    };
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<(bool, Result<Vec<u8>>)>(32);
    if let Some(mut stdin) = child.stdin.take() {
        let input = options.input.clone().unwrap_or_default();
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let mut readers = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let tx = sender.clone();
        readers.push(tokio::spawn(async move {
            loop {
                let mut data = vec![0; 8192];
                match stdout.read(&mut data).await {
                    Ok(0) => break,
                    Ok(n) => {
                        data.truncate(n);
                        if tx.send((false, Ok(data))).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send((false, Err(e.into()))).await;
                        break;
                    }
                }
            }
        }));
    }
    if let Some(mut stderr) = child.stderr.take() {
        let tx = sender.clone();
        readers.push(tokio::spawn(async move {
            loop {
                let mut data = vec![0; 8192];
                match stderr.read(&mut data).await {
                    Ok(0) => break,
                    Ok(n) => {
                        data.truncate(n);
                        if tx.send((true, Ok(data))).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send((true, Err(e.into()))).await;
                        break;
                    }
                }
            }
        }));
    }
    drop(sender);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut pending = Vec::new();
    let mut total = 0;
    let pump = async {
        let mut status = None;
        loop {
            let received = tokio::select! {
                value = child.wait(), if status.is_none() => {
                    status = Some(value?);
                    if options.detached { group.signal(libc::SIGKILL); }
                    continue;
                }
                value = receiver.recv() => value,
            };
            let Some((is_error, data)) = received else {
                break;
            };
            let data = data?;
            if options.capture {
                total += data.len();
                if total > options.max_output {
                    bail!("Process output limit exceeded");
                }
            }
            if is_error {
                stderr.extend_from_slice(&data);
                if stderr.len() > 65536 {
                    stderr.drain(..stderr.len() - 65536);
                }
            } else {
                if options.capture {
                    stdout.extend_from_slice(&data);
                }
                if let Some(callback) = &options.on_line {
                    pending.extend_from_slice(&data);
                    while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                        if end > options.max_output {
                            bail!("Process output line limit exceeded");
                        }
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        callback(
                            String::from_utf8(line[..end].to_vec())
                                .context("Invalid UTF-8 process event")?,
                        )
                        .await?;
                    }
                    if pending.len() > options.max_output {
                        bail!("Process output line limit exceeded");
                    }
                }
            }
        }
        if !pending.is_empty()
            && let Some(callback) = &options.on_line
        {
            callback(String::from_utf8(pending.clone())?).await?;
        }
        let status = match status {
            Some(s) => s,
            None => child.wait().await?,
        };
        if !status.success() {
            bail!(
                "{program} exited {status}: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok(())
    };
    let deadline = async {
        match options.timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending::<()>().await,
        }
    };
    let result: Result<()> = tokio::select! {
        result = pump => result,
        _ = options.cancel.cancelled() => Err(anyhow::anyhow!("Interrupted")),
        _ = deadline => Err(anyhow::anyhow!("{program} timed out")),
    };
    if result.is_err() {
        group.signal(libc::SIGINT);
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            group.signal(libc::SIGKILL);
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    // Descendants can retain the pipes after the direct child exits.
    drop(group);
    for reader in readers {
        reader.abort();
        let _ = reader.await;
    }
    result?;
    Ok(Output {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn arguments_are_literal_and_output_is_bounded() {
        let out = run(
            "printf",
            &["%s".into(), "$(touch /tmp/crow-never)".into()],
            RunOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, "$(touch /tmp/crow-never)");
        assert!(
            run(
                "printf",
                &["12345".into()],
                RunOptions {
                    max_output: 4,
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("limit")
        );
    }
    #[tokio::test]
    async fn exiting_parent_does_not_leave_inherited_pipes_open() {
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            run(
                "sh",
                &["-c".into(), "sleep 30 & printf done".into()],
                RunOptions::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.stdout, "done");
    }
    #[tokio::test]
    async fn timeout_stops_a_silent_process() {
        let start = std::time::Instant::now();
        assert!(
            run(
                "sleep",
                &["30".into()],
                RunOptions {
                    timeout: Some(Duration::from_millis(20)),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(4));
    }
}
