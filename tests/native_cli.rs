//! Exercise the shipped executable, not just library entry points.
use anyhow::{Context, Result, ensure};
use crow::{config, util};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn binary() -> String {
    std::env::var("CROW_TEST_BINARY").unwrap_or_else(|_| env!("CARGO_BIN_EXE_crow").to_owned())
}

#[test]
fn native_help_version_and_invalid_arguments_need_no_runtime() {
    for args in [
        vec!["--version"],
        vec!["version"],
        vec!["help"],
        vec!["--help"],
    ] {
        let output = Command::new(binary())
            .args(&args)
            .env("PATH", "")
            .env("NODE_OPTIONS", "--invalid-option")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
        if args.contains(&"--version") || args.contains(&"version") {
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                env!("CARGO_PKG_VERSION")
            );
        }
    }
    for args in [
        vec!["install", "--unknown"],
        vec!["review", "a/b", "0"],
        vec!["config", "worker.concurrency"],
        vec!["setup", "--role", "wrong"],
    ] {
        assert!(
            !Command::new(binary())
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn service_binary_enforces_auth_and_stops_cleanly() -> Result<()> {
    let root = tempfile::tempdir()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut cfg = config::defaults(root.path());
    cfg["role"] = json!("both");
    cfg["port"] = json!(port);
    cfg["serviceUrl"] = json!(format!("http://127.0.0.1:{port}"));
    cfg["catchUp"]["enabled"] = json!(false);
    config::save(root.path(), &cfg)?;
    let mut child = ChildGuard(
        Command::new(binary())
            .arg("run")
            .env("CROW_HOME", root.path())
            .env("PATH", "")
            .env("NODE_OPTIONS", "--invalid-option")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("ready.json").exists() {
        if let Some(status) = child.0.try_wait()? {
            anyhow::bail!("service exited before readiness: {status}");
        }
        ensure!(Instant::now() < deadline, "service readiness timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(health.is_object());
    let unauthenticated = client.get(format!("{base}/admin/status")).send().await?;
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let status: Value = client
        .get(format!("{base}/admin/status"))
        .bearer_auth(cfg["adminToken"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(status.is_object());
    // A second process must not remove or take over the first process's lock.
    let second = Command::new(binary())
        .arg("run")
        .env("CROW_HOME", root.path())
        .output()?;
    assert!(!second.status.success());
    let ready = util::read_json(&root.path().join("ready.json"))?.unwrap();
    assert_eq!(ready["pid"], child.0.id());
    // Test the actual update coordination API against Linux signals and disk acknowledgments.
    let before_drain =
        crow::operations::worker_drain_state(root.path())?.context("Running worker")?;
    crow::operations::drain_worker(root.path()).await?;
    assert!(root.path().join("drained.json").exists());
    crow::operations::resume_worker(root.path(), &before_drain).await?;
    assert!(!root.path().join("drained.json").exists());
    assert_eq!(
        util::read_json(&root.path().join("undrained.json"))?.unwrap()["pid"],
        child.0.id()
    );
    std::fs::remove_file(root.path().join("undrained.json"))?;
    // Both pending signals must settle in the undrained state.
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGSTOP);
        libc::kill(child.0.id() as i32, libc::SIGUSR1);
        libc::kill(child.0.id() as i32, libc::SIGUSR2);
        libc::kill(child.0.id() as i32, libc::SIGCONT);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.path().join("undrained.json").exists() {
        ensure!(
            Instant::now() < deadline,
            "pending drain signals did not settle"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!root.path().join("drained.json").exists());
    assert_eq!(
        util::read_json(&root.path().join("worker-status.json"))?.unwrap()["state"],
        "running"
    );
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait()? {
            assert!(status.success());
            break;
        }
        ensure!(Instant::now() < deadline, "service shutdown timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!root.path().join("ready.json").exists());
    let db = rusqlite::Connection::open(root.path().join("service.sqlite"))?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    assert_eq!(integrity, "ok");
    Ok(())
}

#[test]
fn private_mcp_entry_uses_the_same_executable() -> Result<()> {
    let root = tempfile::tempdir()?;
    let source_path = root.path().join("source.json");
    util::atomic(
        &source_path,
        &json!({"dir":root.path(),"head":"a".repeat(40),"base":"b".repeat(40),"target":"main","targetSha":"c".repeat(40)}),
    )?;
    let mut child = ChildGuard(
        Command::new(binary())
            .arg("_inspection-mcp")
            .arg(source_path)
            .env("PATH", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let input = child.0.stdin.as_mut().context("stdin")?;
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"native-test","version":"1"}}})
    )?;
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    )?;
    input.flush()?;
    let mut reader = BufReader::new(child.0.stdout.take().context("stdout")?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let initialized: Value = serde_json::from_str(&line)?;
    assert_eq!(initialized["id"], 1);
    assert!(initialized["result"]["capabilities"].is_object());
    line.clear();
    reader.read_line(&mut line)?;
    let tools: Value = serde_json::from_str(&line)?;
    assert_eq!(tools["id"], 2);
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .context("tools")?
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 4);
    assert!(
        !names
            .iter()
            .any(|n| n.contains("exec") || n.contains("write"))
    );
    drop(child.0.stdin.take());
    assert!(child.0.wait()?.success());
    Ok(())
}
