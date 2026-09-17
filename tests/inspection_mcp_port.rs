//! Exercise the actual executable's private MCP transport, not just tool helpers.
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn crow_binary() -> std::ffi::OsString {
    std::env::var_os("CROW_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_crow").into())
}

#[test]
fn mcp_advertises_pagination_and_serializes_errors_without_executing_tools() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    std::fs::write(&source,json!({"dir":dir.path(),"head":"a".repeat(40),"base":"b".repeat(40),"targetSha":"b".repeat(40)}).to_string()).unwrap();
    let mut child = Command::new(crow_binary())
        .arg("_inspection-mcp")
        .arg(source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"shell","arguments":{"command":"touch SHOULD_NOT_EXIST"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":5,"method":"unsupported"}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"../secret"}}}),
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"not-json\n").unwrap();
        for request in requests {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0]["result"]["protocolVersion"], "2024-11-05");
    let definitions = messages[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(definitions.len(), 4);
    for name in ["list_files", "diff"] {
        let tool = definitions.iter().find(|t| t["name"] == name).unwrap();
        assert_eq!(tool["inputSchema"]["properties"]["offset"]["minimum"], 0);
        assert!(tool["description"].as_str().unwrap().contains("nextOffset"));
    }
    assert_eq!(messages[2]["result"]["isError"], true);
    assert_eq!(
        messages[2]["result"]["content"][0]["text"],
        "Unknown inspection tool"
    );
    assert_eq!(messages[3]["result"], json!({}));
    assert_eq!(messages[4]["error"]["code"], -32601);
    assert_eq!(
        messages[5]["result"]["content"][0]["text"],
        "Invalid repository path"
    );
    assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
}

#[tokio::test]
async fn idle_mcp_exits_on_both_signals_without_stdin_eof() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for signal in [libc::SIGINT, libc::SIGTERM] {
        for partial in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.json");
            std::fs::write(&source, b"{}").unwrap();
            let mut child = tokio::process::Command::new(crow_binary())
                .arg("_inspection-mcp")
                .arg(source)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            // Child::wait closes a stored stdin handle. Keep ownership here so
            // success proves runtime shutdown, without any implicit input EOF.
            let mut input = child.stdin.take().unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let test = async {
                input
                    .write_all(b"{\"id\":1,\"method\":\"ping\"}\n")
                    .await
                    .unwrap();
                let mut response = String::new();
                output.read_line(&mut response).await.unwrap();
                assert_eq!(serde_json::from_str::<Value>(&response).unwrap()["id"], 1);
                if partial {
                    input.write_all(b"{\"id\":2,\"method\":").await.unwrap();
                }
                // Let the next input read become pending before signalling.
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(unsafe { libc::kill(child.id().unwrap() as i32, signal) }, 0);
                child.wait().await.unwrap()
            };
            match tokio::time::timeout(Duration::from_secs(3), test).await {
                Ok(status) => assert!(
                    status.success(),
                    "signal {signal}, partial={partial}: {status}"
                ),
                Err(_) => {
                    child.kill().await.unwrap();
                    child.wait().await.unwrap();
                    panic!(
                        "MCP did not exit with stdin open after signal {signal}, partial={partial}"
                    );
                }
            }
            drop(input);
        }
    }
}

#[test]
fn redirected_regular_file_input_preserves_mcp_frame_limit() {
    use std::io::Seek;
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.json");
    std::fs::write(&source, b"{}").unwrap();
    for extra in [0, 1] {
        let mut request = b"{\"id\":1,\"method\":\"ping\"}".to_vec();
        request.resize(4 * 1024 * 1024 - 1 + extra, b' ');
        request.push(b'\n');
        // Two following frames must remain independently readable, including
        // a final complete JSON value without a trailing newline.
        request
            .extend_from_slice(b"{\"id\":2,\"method\":\"ping\"}\n{\"id\":3,\"method\":\"ping\"}");
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&request).unwrap();
        file.rewind().unwrap();
        let output = Command::new(crow_binary())
            .arg("_inspection-mcp")
            .arg(&source)
            .stdin(file)
            .output()
            .unwrap();
        if extra == 0 {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let messages: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                messages
                    .iter()
                    .map(|v| v["id"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
        } else {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("MCP request exceeds 4 MiB"));
            assert!(output.stdout.is_empty());
        }
    }
}

#[tokio::test]
async fn mcp_exits_on_signals_while_stdout_pipe_is_full() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.json");
        std::fs::write(&source, b"{}").unwrap();
        let mut child = tokio::process::Command::new(crow_binary())
            .arg("_inspection-mcp")
            .arg(source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let test = async {
            input
                .write_all(b"{\"id\":1,\"method\":\"ping\"}\n")
                .await
                .unwrap();
            let mut response = String::new();
            output.read_line(&mut response).await.unwrap();
            assert_eq!(serde_json::from_str::<Value>(&response).unwrap()["id"], 1);
            // Echoing this valid ID fills the response pipe. Keep the reader
            // alive but stop consuming, so shutdown cannot rely on EPIPE.
            let request = json!({"id":"x".repeat(2*1024*1024),"method":"ping"});
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(unsafe { libc::kill(child.id().unwrap() as i32, signal) }, 0);
            child.wait().await.unwrap()
        };
        match tokio::time::timeout(Duration::from_secs(5), test).await {
            Ok(status) => assert!(status.success(), "signal {signal}: {status}"),
            Err(_) => {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                panic!("MCP did not exit after signal {signal} while stdout was full");
            }
        }
        drop(input);
        drop(output);
    }
}

#[tokio::test]
async fn healthy_mcp_stream_delivers_large_responses_and_later_frames() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.json");
    std::fs::write(&source, b"{}").unwrap();
    let mut child = tokio::process::Command::new(crow_binary())
        .arg("_inspection-mcp")
        .arg(source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let test = async {
        let id = "x".repeat(2 * 1024 * 1024);
        for request_id in [json!(id), json!(2)] {
            input
                .write_all(format!("{}\n", json!({"id":request_id,"method":"ping"})).as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            output.read_line(&mut response).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&response).unwrap(),
                json!({"jsonrpc":"2.0","id":request_id,"result":{}})
            );
        }
        drop(input);
        assert!(child.wait().await.unwrap().success());
    };
    tokio::time::timeout(Duration::from_secs(5), test)
        .await
        .unwrap();
}

#[test]
fn redirected_stdout_writes_complete_frames_and_restores_flags() {
    use std::{
        io::{Read, Seek},
        os::fd::AsRawFd,
    };
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.json");
    std::fs::write(&source, b"{}").unwrap();
    let mut input = tempfile::tempfile().unwrap();
    input
        .write_all(b"{\"id\":1,\"method\":\"ping\"}\n")
        .unwrap();
    input.rewind().unwrap();
    let mut output = tempfile::tempfile().unwrap();
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    let status = Command::new(crow_binary())
        .arg("_inspection-mcp")
        .arg(source)
        .stdin(input)
        .stdout(output.try_clone().unwrap())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) },
        flags
    );
    output.rewind().unwrap();
    let mut response = String::new();
    output.read_to_string(&mut response).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap(),
        json!({"jsonrpc":"2.0","id":1,"result":{}})
    );
}
