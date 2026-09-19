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
async fn queued_large_artifacts_are_drained_before_the_next_tool_starts() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = tempfile::tempdir().unwrap();
    let experiments = directory.path().join("experiments");
    let artifacts = experiments.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    let id = "a".repeat(32);
    let mut artifact_paths = Vec::new();
    for index in 0..3 {
        let path = artifacts.join(format!("{id}-{index}.png"));
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, 1397, 1000);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().unwrap();
        let mut data = Vec::with_capacity(1397 * 1000 * 3);
        let mut state = 0x9e37_79b9_u32.wrapping_add(index as u32);
        for _ in 0..(1397 * 1000 * 3) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            data.push((state >> 24) as u8);
        }
        writer.write_image_data(&data).unwrap();
        artifact_paths.push(path);
    }
    let source = json!({"dir":directory.path(),"head":"a".repeat(40),"base":"b".repeat(40)});
    std::fs::write(directory.path().join("source.json"), source.to_string()).unwrap();
    let context = json!({"root":directory.path(),"source":source,"job":{"repo":"owner/repo","settings":{"execution":{"automatic":true}}}});
    std::fs::write(directory.path().join("context.json"), context.to_string()).unwrap();
    let artifacts_json: Vec<Value> = artifact_paths
        .iter()
        .map(|path| json!({"path":path,"saved":true}))
        .collect();
    std::fs::write(experiments.join(format!("{id}.json")), json!({"id":id,"status":"passed","commit":"a".repeat(40),"revision":"head","command":"capture","artifacts":artifacts_json}).to_string()).unwrap();
    std::os::unix::fs::symlink(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/slow-discovery-git.py"),
        directory.path().join("git"),
    )
    .unwrap();
    let env_paths = std::iter::once(directory.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>(),
    );
    let mut child = tokio::process::Command::new(crow_binary())
        .arg("_inspection-mcp")
        .arg(directory.path().join("source.json"))
        .arg(directory.path().join("context.json"))
        .env("PATH", std::env::join_paths(env_paths).unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"{\"id\":0,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"package.json\"}}}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !directory.path().join("git-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Delayed inspection did not start");
    // Seven accepted reads are queued behind an actual blocked Git read. Six
    // near-limit PNG responses together exceed the 16 MiB output cap twice.
    for index in 0..7 {
        input.write_all(format!("{}\n", json!({"id":index+1,"method":"tools/call","params":{"name":"read_artifact","arguments":{"experiment":id,"index":index % 3}}})).as_bytes()).await.unwrap();
    }
    input
        .write_all(b"{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":0}}\n")
        .await
        .unwrap();
    let initial: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(5), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(initial["id"], 0);
    assert_eq!(initial["result"]["isError"], true);
    // Stop reading while the first large PNG fills stdout. The transport must
    // still process cancellation of a queued read before starting that read.
    input
        .write_all(b"{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":7}}\n")
        .await
        .unwrap();
    for index in 0..6 {
        let line = tokio::time::timeout(Duration::from_secs(15), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], index + 1, "response ordering/backpressure");
        assert_eq!(
            response["result"]["content"][1]["type"], "image",
            "{response}"
        );
        assert!(
            response["result"]["content"][1]["data"]
                .as_str()
                .unwrap()
                .len()
                > 5 * 1024 * 1024
        );
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response["result"]["content"][1]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, std::fs::read(&artifact_paths[index % 3]).unwrap());
    }
    input
        .write_all(b"{\"id\":99,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    let ping: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(3), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ping, json!({"jsonrpc":"2.0","id":99,"result":{}}));
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
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
