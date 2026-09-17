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

/// Cancellable I/O for a descriptor that would otherwise block a Tokio worker.
/// The caller must be its only reader/writer while this guard exists. Original
/// file status flags are restored before another prompt or child uses the fd.
/// Regular files cannot register with epoll, so they use direct nonblocking I/O.
pub(crate) struct NonblockingIo<'a> {
    fd: std::os::fd::BorrowedFd<'a>,
    flags: libc::c_int,
    readiness: Option<tokio::io::unix::AsyncFd<std::os::fd::BorrowedFd<'a>>>,
    retry: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<'a> NonblockingIo<'a> {
    pub(crate) fn new(fd: std::os::fd::BorrowedFd<'a>) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        let mut io = Self {
            fd,
            flags,
            readiness: None,
            retry: None,
        };
        match tokio::io::unix::AsyncFd::new(fd) {
            Ok(readiness) => io.readiness = Some(readiness),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {}
            Err(error) => return Err(error),
        }
        Ok(io)
    }

    fn poll_io(
        &mut self,
        cx: &mut std::task::Context<'_>,
        write: bool,
        mut operation: impl FnMut(libc::c_int) -> std::io::Result<usize>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::{
            os::fd::AsRawFd,
            task::{Poll, ready},
        };
        if let Some(readiness) = &self.readiness {
            loop {
                let mut ready = ready!(if write {
                    readiness.poll_write_ready(cx)
                } else {
                    readiness.poll_read_ready(cx)
                })?;
                match operation(self.fd.as_raw_fd()) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        ready.clear_ready()
                    }
                    result => return Poll::Ready(result),
                }
            }
        }
        loop {
            if let Some(retry) = &mut self.retry {
                if retry.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                self.retry = None;
            }
            match operation(self.fd.as_raw_fd()) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.retry = Some(Box::pin(tokio::time::sleep(Duration::from_millis(20))));
                }
                result => return Poll::Ready(result),
            }
        }
    }
}

impl NonblockingIo<'static> {
    pub(crate) fn stdin() -> std::io::Result<Self> {
        // The process owns its standard descriptors for its lifetime. Borrow
        // them without closing them when an I/O guard is dropped.
        Self::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(libc::STDIN_FILENO) })
    }
    pub(crate) fn stdout() -> std::io::Result<Self> {
        Self::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(libc::STDOUT_FILENO) })
    }
}

impl tokio::io::AsyncRead for NonblockingIo<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::{Poll, ready};
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let available = buffer.initialize_unfilled();
        let count = ready!(self.get_mut().poll_io(cx, false, |fd| {
            let count = unsafe { libc::read(fd, available.as_mut_ptr().cast(), available.len()) };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }))?;
        buffer.advance(count);
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for NonblockingIo<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if bytes.is_empty() {
            return std::task::Poll::Ready(Ok(0));
        }
        self.get_mut().poll_io(cx, true, |fd| {
            let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Writes go directly to the descriptor; this type has no output buffer.
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl Drop for NonblockingIo<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, self.flags) };
    }
}

type LineFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type LineCallback = Arc<dyn Fn(String) -> LineFuture + Send + Sync>;

async fn dispatch_line(
    callback: &LineCallback,
    line: String,
    accepted: &mut Option<LineFuture>,
) -> Result<()> {
    *accepted = Some(callback(line));
    let result = accepted.as_mut().expect("Accepted process callback").await;
    // A completed future must never be polled again, including after an error.
    *accepted = None;
    result
}
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
    // Keep the accepted callback outside the cancellable pump. A session save
    // may have persisted a snapshot but still need to publish it in memory.
    let mut accepted_callback = None;
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
                        dispatch_line(
                            callback,
                            String::from_utf8(line[..end].to_vec())
                                .context("Invalid UTF-8 process event")?,
                            &mut accepted_callback,
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
            dispatch_line(
                callback,
                String::from_utf8(pending.clone())?,
                &mut accepted_callback,
            )
            .await?;
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
    let cleanup = async {
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
    };
    let drain_callback = async {
        if let Some(callback) = accepted_callback {
            callback.await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    // Stop the provider promptly, independently of durable callback work. Only
    // the already accepted callback drains; queued output cannot admit more.
    // Its completion orders the caller's final save after the session save.
    let (_, drained) = tokio::join!(cleanup, drain_callback);
    drained.context("Accepted process event callback failed during shutdown")?;
    result?;
    Ok(Output {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn nonblocking_input_restores_flags_and_supports_regular_files() {
        use std::{
            io::{Seek, Write},
            os::fd::{AsFd, AsRawFd},
        };
        let mut file = tempfile::tempfile().unwrap();
        let expected = "file input 尾\n".repeat(1024);
        file.write_all(expected.as_bytes()).unwrap();
        file.rewind().unwrap();
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        {
            let mut input = NonblockingIo::new(file.as_fd()).unwrap();
            let mut actual = String::new();
            input.read_to_string(&mut actual).await.unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }

    #[tokio::test]
    async fn cancelled_input_leaves_no_reader_and_restores_existing_flags() {
        use std::os::{
            fd::{AsFd, AsRawFd},
            unix::net::UnixStream,
        };
        for already_nonblocking in [false, true] {
            let (read, _write) = UnixStream::pair().unwrap();
            read.set_nonblocking(already_nonblocking).unwrap();
            let flags = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) };
            {
                let mut input = NonblockingIo::new(read.as_fd()).unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), input.read(&mut [0]))
                        .await
                        .is_err()
                );
            }
            assert_eq!(
                unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) },
                flags
            );
        }
    }
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
    async fn cancellation_and_timeout_finish_accepted_save_before_returning() {
        use serde_json::{Value, json};
        use std::sync::{
            Mutex,
            atomic::{AtomicU32, Ordering},
        };
        use tokio::sync::Notify;
        for (timed_out, fail_callback) in [(false, false), (true, false), (false, true)] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("task.json");
            let task = Arc::new(Mutex::new(json!({"state":"running"})));
            let written = Arc::new(Notify::new());
            let commit = Arc::new(Notify::new());
            let pid = Arc::new(AtomicU32::new(0));
            let on_line: LineCallback = {
                let (path, task, written, commit, pid) = (
                    path.clone(),
                    task.clone(),
                    written.clone(),
                    commit.clone(),
                    pid.clone(),
                );
                Arc::new(move |line| {
                    let (path, task, written, commit, pid) = (
                        path.clone(),
                        task.clone(),
                        written.clone(),
                        commit.clone(),
                        pid.clone(),
                    );
                    Box::pin(async move {
                        let event: Value = serde_json::from_str(&line)?;
                        pid.store(event["pid"].as_u64().unwrap() as u32, Ordering::SeqCst);
                        let mut snapshot = task.lock().unwrap().clone();
                        snapshot["session"] = event["thread_id"].clone();
                        tokio::fs::write(path, serde_json::to_vec(&snapshot)?).await?;
                        written.notify_one();
                        // Simulate an atomic save waiting to publish its snapshot
                        // in memory. Final pause persistence must not overtake it.
                        commit.notified().await;
                        *task.lock().unwrap() = snapshot;
                        if fail_callback {
                            bail!("Session callback rejected");
                        }
                        Ok(())
                    })
                })
            };
            let cancel = CancellationToken::new();
            let operation = {
                let (cancel, path, task) = (cancel.clone(), path.clone(), task.clone());
                tokio::spawn(async move {
                    let result = run("sh", &["-c".into(), r#"printf '{"type":"thread.started","thread_id":"saved-session","pid":%s}\n' "$$"; exec sleep 30"#.into()], RunOptions {
                        cancel,
                        timeout: timed_out.then_some(Duration::from_secs(1)),
                        on_line: Some(on_line),
                        ..Default::default()
                    }).await;
                    let mut final_snapshot = task.lock().unwrap().clone();
                    final_snapshot["state"] = json!("paused");
                    crate::util::atomic(&path, &final_snapshot).unwrap();
                    result
                })
            };
            tokio::time::timeout(Duration::from_secs(3), written.notified())
                .await
                .unwrap();
            if !timed_out {
                cancel.cancel();
            }
            // The child must stop even though its accepted callback is blocked.
            tokio::time::timeout(Duration::from_secs(4), async {
                while unsafe { libc::kill(pid.load(Ordering::SeqCst) as i32, 0) } == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("Process cleanup waited for the callback");
            assert!(
                !operation.is_finished(),
                "Process returned before its accepted callback committed"
            );
            commit.notify_one();
            let error = tokio::time::timeout(Duration::from_secs(3), operation)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            if fail_callback {
                assert!(format!("{error:#}").contains("Session callback rejected"));
            } else {
                assert!(error.to_string().contains(if timed_out {
                    "timed out"
                } else {
                    "Interrupted"
                }));
            }
            assert_eq!(
                crate::util::read_json(&path).unwrap().unwrap(),
                json!({"state":"paused","session":"saved-session"})
            );
        }
    }

    #[tokio::test]
    async fn accepted_callbacks_keep_line_order_and_errors_are_not_polled_twice() {
        use std::sync::Mutex;
        for fail in [false, true] {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let received = lines.clone();
            let callback: LineCallback = Arc::new(move |line| {
                let received = received.clone();
                Box::pin(async move {
                    received.lock().unwrap().push(line.clone());
                    if fail && line == "second" {
                        bail!("Rejected process event");
                    }
                    tokio::task::yield_now().await;
                    Ok(())
                })
            });
            let result = run(
                "printf",
                &["first\nsecond\ntrailing".into()],
                RunOptions {
                    on_line: Some(callback),
                    ..Default::default()
                },
            )
            .await;
            if fail {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Rejected process event")
                );
                assert_eq!(*lines.lock().unwrap(), ["first", "second"]);
            } else {
                result.unwrap();
                assert_eq!(*lines.lock().unwrap(), ["first", "second", "trailing"]);
            }
        }
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
