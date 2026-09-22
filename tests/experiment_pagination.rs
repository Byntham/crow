//! Saved experiment logs must remain readable through the actual MCP transport.
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout},
};

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

async fn request(
    input: &mut ChildStdin,
    output: &mut Lines<BufReader<ChildStdout>>,
    request: Value,
) -> Value {
    input
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let line = tokio::time::timeout(Duration::from_secs(10), output.next_line())
        .await
        .expect("MCP response timed out")
        .unwrap()
        .expect("MCP connection closed before responding");
    assert!(
        line.len() + 1 < 16 * 1024 * 1024,
        "MCP response exceeded the transport frame budget"
    );
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], request["id"]);
    assert!(response.get("error").is_none(), "{}", response["error"]);
    response
}

fn result_json(response: &Value) -> Value {
    assert_ne!(
        response["result"]["isError"], true,
        "{}",
        response["result"]["content"][0]["text"]
    );
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn full_experiment_logs_paginate_without_closing_the_mcp_connection() {
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("test.txt"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let source = json!({"dir":repo,"head":commit,"base":commit});
    let source_path = directory.path().join("source.json");
    std::fs::write(&source_path, source.to_string()).unwrap();
    let policy = json!({
        "image":format!("sha256:{}", "b".repeat(64)),
        "timeoutSeconds":30,"memoryMiB":1024,"workspaceMiB":512,
        "cpus":2,"pids":128,"maxRuns":50
    });
    let context_path = directory.path().join("context.json");
    std::fs::write(
        &context_path,
        json!({
            "root":directory.path(),"source":source,
            "job":{"repo":"fixture/repo","settings":{"execution":{
                "podman":directory.path().join("must-not-run-podman"),
                "repositories":{"fixture/repo":policy}
            }}}
        })
        .to_string(),
    )
    .unwrap();

    // inspection_main resolves receipts beside source.json, independently of
    // the repository directory. Terminal receipts need no container cleanup.
    let experiments = directory.path().join("experiments");
    std::fs::create_dir(&experiments).unwrap();
    let log = "\0".repeat(32 * 1024);
    let mut expected = Vec::new();
    for index in (0..50).rev() {
        let id = format!("{index:032x}");
        let receipt = json!({
            "id":id,"status":"passed","containerStarted":false,
            "revision":"head","commit":commit,"image":policy["image"],
            "command":format!("fixture-{index}"),"phase":"test","limits":policy,
            "startedAt":format!("2026-09-21T00:00:{:02}Z", index % 5),
            "exitCode":0,"stdout":log,"stderr":log,"artifacts":[]
        });
        std::fs::write(experiments.join(format!("{id}.json")), receipt.to_string()).unwrap();
        expected.push(receipt);
    }
    // Control characters expand when JSON encoded. Fifty legal stdout/stderr
    // pairs already exceed the MCP limit before the outer text-content wrapper.
    assert!(serde_json::to_vec(&expected).unwrap().len() > 16 * 1024 * 1024);
    expected.sort_by(|a, b| {
        (a["startedAt"].as_str(), a["id"].as_str())
            .cmp(&(b["startedAt"].as_str(), b["id"].as_str()))
    });

    let binary =
        std::env::var_os("CROW_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_crow").into());
    let mut child = tokio::process::Command::new(binary)
        .arg("_inspection-mcp")
        .arg(source_path)
        .arg(context_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let definitions = request(
        &mut input,
        &mut output,
        json!({"id":"schema","method":"tools/list"}),
    )
    .await;
    let tool = definitions["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "list_experiments")
        .unwrap();

    let mut offset = 0;
    let mut pages = 0;
    let mut seen = HashSet::new();
    loop {
        // The default call must also return a bounded first page.
        let arguments = if offset == 0 {
            json!({})
        } else {
            json!({"offset":offset,"count":50})
        };
        let page = result_json(
            &request(
                &mut input,
                &mut output,
                json!({"id":pages,"method":"tools/call","params":{
                    "name":"list_experiments","arguments":arguments
                }}),
            )
            .await,
        );
        assert_eq!(page["policy"], policy);
        assert_eq!(page["total"], 50);
        assert_eq!(page["offset"], offset);
        let runs = page["runs"].as_array().unwrap();
        assert!(!runs.is_empty());
        assert!(runs.len() <= 50);
        assert!(
            runs.iter()
                .map(|run| serde_json::to_vec(run).unwrap().len())
                .sum::<usize>()
                <= 1024 * 1024
        );
        for (index, receipt) in runs.iter().enumerate() {
            let id = receipt["id"].as_str().unwrap();
            assert!(seen.insert(id.to_owned()), "Duplicate receipt {id}");
            assert!(receipt == &expected[offset + index], "Receipt {id} changed");
        }
        offset += runs.len();
        pages += 1;
        assert!(offset <= expected.len());
        assert_eq!(page["truncated"], offset < expected.len());
        if offset == expected.len() {
            assert!(page["nextOffset"].is_null());
            break;
        }
        assert_eq!(page["nextOffset"], offset);
    }
    assert!(pages > 1);
    assert_eq!(seen.len(), expected.len());
    assert_eq!(tool["inputSchema"]["properties"]["offset"]["minimum"], 0);
    assert_eq!(tool["inputSchema"]["properties"]["count"]["minimum"], 1);
    assert_eq!(tool["inputSchema"]["properties"]["count"]["maximum"], 50);
    assert!(tool["description"].as_str().unwrap().contains("nextOffset"));

    let single = result_json(
        &request(
            &mut input,
            &mut output,
            json!({"id":"single","method":"tools/call","params":{
                "name":"list_experiments","arguments":{"offset":1,"count":1}
            }}),
        )
        .await,
    );
    let runs = single["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0] == expected[1],
        "Repeated page changed receipt order"
    );
    assert_eq!(single["nextOffset"], 2);
    assert_eq!(single["truncated"], true);

    for offset in [50, 500] {
        let page = result_json(
            &request(
                &mut input,
                &mut output,
                json!({"id":"end","method":"tools/call","params":{
                    "name":"list_experiments","arguments":{"offset":offset,"count":1}
                }}),
            )
            .await,
        );
        assert_eq!(page["runs"], json!([]));
        assert_eq!(page["policy"], policy);
        assert_eq!(page["total"], 50);
        assert_eq!(page["truncated"], false);
        assert!(page["nextOffset"].is_null());
    }
    for arguments in [
        json!({"offset":-1}),
        json!({"offset":1.5}),
        json!({"offset":"0"}),
        json!({"count":0}),
        json!({"count":51}),
        json!({"count":1.5}),
        json!({"count":"1"}),
        json!({"unexpected":true}),
        json!([]),
    ] {
        let response = request(
            &mut input,
            &mut output,
            json!({"id":"invalid","method":"tools/call","params":{
                "name":"list_experiments","arguments":arguments
            }}),
        )
        .await;
        assert_eq!(response["result"]["isError"], true);
    }

    let read = request(
        &mut input,
        &mut output,
        json!({"id":"read","method":"tools/call","params":{
            "name":"read_file","arguments":{"path":"test.txt"}
        }}),
    )
    .await;
    assert_ne!(read["result"]["isError"], true);
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fixture")
    );
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
