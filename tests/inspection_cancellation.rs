//! MCP cancellation must reach the running tool while stdin remains open.
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

async fn scenario(eof: bool, queued_eof: bool, provider_kill: bool) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("test.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let runtime = root.path().join("runtime");
    // Invoke an immutable executable through a per-test symlink. Concurrent
    // process spawns cannot inherit a descriptor opened for writing its image.
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-runtime.py"),
        &runtime,
    )
    .unwrap();
    let source = root.path().join("source.json");
    let pinned = json!({"dir":repo,"head":commit,"base":commit});
    std::fs::write(&source, pinned.to_string()).unwrap();
    let context = root.path().join("context.json");
    std::fs::write(&context, json!({"root":root.path(),"source":pinned,"job":{"repo":"fixture/repo","settings":{"execution":{"podman":runtime,"repositories":{"fixture/repo":{"image":format!("sha256:{}", "b".repeat(64)),"timeoutSeconds":30}}}}}}).to_string()).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crow"))
        .arg("_inspection-mcp")
        .arg(source)
        .arg(&context)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    if queued_eof {
        // read_file awaits Git while the following request and EOF are read.
        // The queued runtime must not start after that disconnect.
        input.write_all(format!("{}\n{}\n",
            json!({"id":1,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"test.txt"}}}),
            json!({"id":2,"method":"tools/call","params":{"name":"run_experiment","arguments":{"revision":"head","command":"fixture-wait"}}}),
        ).as_bytes()).await.unwrap();
        drop(input);
        let response: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(10), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["id"], 1);
        assert_ne!(response["result"]["isError"], true, "{response}");
        assert!(
            tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(
            !root.path().join("created").exists(),
            "Queued runtime started after stdin EOF"
        );
        return;
    }
    input.write_all(format!("{}\n", json!({"id":7,"method":"tools/call","params":{"name":"run_experiment","arguments":{"revision":"head","command":"fixture-wait"}}})).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("active").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runtime command never started");
    if provider_kill {
        let environments = root.path().join("experiments/environments");
        std::fs::create_dir_all(&environments).unwrap();
        let snapshot = environments.join("prepared.tar");
        std::fs::write(&snapshot, "paused review snapshot").unwrap();
        std::fs::write(root.path().join("hold-remove"), "").unwrap();
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
            0
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.path().join("removing").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP did not begin cleanup");
        // Model provider-group termination while MCP's Podman rm is still blocked.
        assert_eq!(
            unsafe { libc::kill(-(child.id().unwrap() as i32), libc::SIGKILL) },
            0
        );
        child.wait().await.unwrap();
        // Podman commands use their own process group. Simulate loss of that
        // removal helper too, rather than letting it finish behind the test.
        let removal_pid: i32 = std::fs::read_to_string(root.path().join("removing-pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(-removal_pid, libc::SIGKILL) }, 0);
        std::fs::remove_file(root.path().join("hold-remove")).unwrap();
        assert!(!root.path().join("removed").exists());
        let context: Value = serde_json::from_slice(&std::fs::read(&context).unwrap()).unwrap();
        let execution =
            crow::execution::Execution::from_context(&context, &root.path().join("experiments"))
                .unwrap()
                .unwrap();
        std::fs::write(root.path().join("fail-remove"), "").unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.path().join("experiments/execution.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let cleanup = execution.cleanup_after_provider();
        tokio::pin!(cleanup);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut cleanup)
                .await
                .is_err(),
            "Parent must wait for the dying MCP's lock"
        );
        assert!(!root.path().join("removed").exists());
        fs2::FileExt::unlock(&lock).unwrap();
        assert!(cleanup.await.is_err());
        let receipt_path = std::fs::read_dir(root.path().join("experiments"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        assert_eq!(receipt["status"], "interrupted");
        assert!(
            receipt["cleanupError"]
                .as_str()
                .unwrap()
                .contains("fixture removal failed")
        );
        std::fs::remove_file(root.path().join("fail-remove")).unwrap();
        execution.cleanup_after_provider().await.unwrap();
        assert!(root.path().join("removed").exists());
        let recovered: Value =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        assert!(recovered.get("cleanupError").is_none());
        assert!(recovered["cleanupRecoveredAt"].is_number());
        assert_eq!(
            std::fs::read_to_string(root.path().join("removed-names")).unwrap(),
            format!("crow-experiment-{}\n", recovered["id"].as_str().unwrap())
        );
        assert!(
            snapshot.exists(),
            "Paused review snapshots must survive provider cleanup"
        );
        return;
    }
    if !eof {
        // A string request ID must not cancel a request with a numeric ID.
        input.write_all(b"{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"7\"}}\n{\"method\":\"notifications/initialized\"}\n").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), output.next_line())
                .await
                .is_err()
        );
        assert!(!root.path().join("removed").exists());
        // Preserve the queued ping and a partially read next message when the
        // active call completes. Only the matching notification cancels it.
        input.write_all(b"{\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"run_experiment\",\"arguments\":{\"revision\":\"head\",\"command\":\"must-not-run\"}}}\n{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":10}}\n{\"id\":8,\"method\":\"ping\"}\n{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":7}}\n{\"id\":9,").await.unwrap();
        let response: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(10), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["id"], 7);
        let receipt: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(receipt["status"], "interrupted", "{receipt}");
        assert!(
            root.path().join("removed").exists(),
            "cleanup must finish before the response"
        );
        let queued: Value =
            serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(queued["id"], 8);
        input.write_all(b"\"method\":\"tools/call\",\"params\":{\"name\":\"list_experiments\",\"arguments\":{}}}\n").await.unwrap();
        let next: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(3), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(next["id"], 9);
        assert_ne!(next["result"]["isError"], true, "{next}");
    }
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(root.path().join("removed").exists());
    let receipts: Vec<Value> = std::fs::read_dir(root.path().join("experiments"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect();
    assert_eq!(
        receipts.len(),
        1,
        "Cancelled queued request must never start"
    );
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt["status"] == "interrupted"),
        "{receipts:?}"
    );
}

#[tokio::test]
async fn mcp_cancellation_matches_request_id_awaits_cleanup_and_preserves_next_requests() {
    scenario(false, false, false).await;
}

#[tokio::test]
async fn mcp_stdin_eof_cancels_active_runtime_and_awaits_cleanup() {
    scenario(true, false, false).await;
}

#[tokio::test]
async fn mcp_stdin_eof_during_inspection_does_not_start_queued_runtime() {
    scenario(false, true, false).await;
}

#[tokio::test]
async fn parent_recovers_after_provider_kills_mcp_during_slow_container_removal() {
    scenario(false, false, true).await;
}
