//! Runtime experiments through the tool interface, with a fake Podman that runs
//! commands in plain directories. Real isolation is covered by the ignored
//! tests in `runtime_podman.rs`.
use crow::execution::Execution;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A repository whose base and head differ in app.txt, served from a bare clone.
fn source(root: &Path) -> Value {
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("app.txt"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("app.txt"), "head\n").unwrap();
    git(&work, &["commit", "-qam", "head"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    let bare = root.join("source.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    json!({"dir": bare, "head": head, "base": base, "target": "main", "targetSha": base})
}

fn fake_podman(root: &Path) -> std::path::PathBuf {
    let state = root.join("podman");
    std::fs::create_dir(&state).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_podman.py");
    let script = root.join("podman.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nFAKE_PODMAN_STATE='{}' exec python3 '{}' \"$@\"\n",
            state.display(),
            fixture.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn open(root: &Path, podman: &Path, source: &Value) -> Execution {
    let context = json!({
        "root": root,
        "job": {"id": "review1", "settings": {"execution": {
            "podman": podman,
            "limits": {"timeoutSeconds": 30, "maxRuns": 5},
        }}},
        "source": source,
    });
    Execution::from_context(&context).unwrap().unwrap()
}

async fn call(execution: &Execution, name: &str, args: Value) -> Value {
    execution.call(name, &args).await.unwrap()
}

#[tokio::test]
async fn experiments_prepare_reuse_and_account_for_every_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = source(root);
    let podman = fake_podman(root);
    let execution = open(root, &podman, &source);

    // Without the runtime image, experiments are refused without using an attempt.
    let info = call(&execution, "runtime_info", json!({})).await;
    assert_eq!(info["imageReady"], false);
    let refused = execution
        .call(
            "run_experiment",
            &json!({"revision":"head","command":"true","purpose":"Probe"}),
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("not ready"), "{refused}");
    std::fs::write(root.join("podman/image"), "").unwrap();

    // Setup saves the workspace for later tests of that revision.
    let setup = call(
        &execution,
        "prepare_environment",
        json!({"revision":"head","command":"echo installed > dep.txt","purpose":"Install"}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    assert_eq!(setup["commit"], source["head"]);
    let test = call(
        &execution,
        "run_experiment",
        json!({"revision":"head","command":"cat dep.txt app.txt","purpose":"Use the installed dependency"}),
    )
    .await;
    assert_eq!(test["status"], "passed", "{test}");
    assert_eq!(test["preparedEnvironment"], true);
    assert_eq!(test["stdout"], "installed\nhead\n");

    // A fresh run, or the other revision, starts from the plain pinned source.
    let fresh = call(
        &execution,
        "run_experiment",
        json!({"revision":"head","command":"test -f dep.txt","purpose":"Fresh","fresh":true}),
    )
    .await;
    assert_eq!(fresh["status"], "failed");
    assert_eq!(fresh["exitCode"], 1);
    let base = call(
        &execution,
        "run_experiment",
        json!({"revision":"base","command":"cat app.txt","purpose":"Same check before the PR"}),
    )
    .await;
    assert_eq!(base["stdout"], "base\n");
    assert_eq!(base["preparedEnvironment"], false);

    // Every container used the isolation flags; only setup had the gateway mount.
    let calls: Vec<Vec<String>> = std::fs::read_to_string(root.join("podman/calls.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let runs: Vec<_> = calls.iter().filter(|c| c[0] == "run").collect();
    assert_eq!(runs.len(), 4);
    for run in &runs {
        assert!(
            run.iter().any(|a| a.starts_with("--security-opt=seccomp=")),
            "seccomp profile missing from {run:?}"
        );
        for flag in [
            "--network=none",
            "--user=1000:1000",
            "--cap-drop=ALL",
            "--read-only",
            "--rm",
        ] {
            assert!(
                run.contains(&flag.to_owned()),
                "{flag} missing from {run:?}"
            );
        }
    }
    let mounts: Vec<_> = runs
        .iter()
        .map(|run| run.iter().any(|a| a.starts_with("--volume=")))
        .collect();
    assert_eq!(mounts, vec![true, false, false, false]);

    // The fifth attempt is the last one; then the budget is spent.
    let invalid = execution
        .call(
            "run_experiment",
            &json!({"revision":"main","command":"true","purpose":"x"}),
        )
        .await;
    assert!(invalid.is_err());
    call(
        &execution,
        "run_experiment",
        json!({"revision":"head","command":"exit 3","purpose":"Fifth"}),
    )
    .await;
    let spent = execution
        .call(
            "run_experiment",
            &json!({"revision":"head","command":"true","purpose":"Sixth"}),
        )
        .await
        .unwrap_err();
    assert!(
        spent.to_string().contains("all 5 experiment attempts"),
        "{spent}"
    );
    let read = call(&execution, "read_experiment", json!({"id":"2"})).await;
    assert_eq!(read["stdout"], "installed\nhead\n");

    let summary = crow::execution::summary(&root.join("reviews/review1")).unwrap();
    assert!(summary.contains("Tests: 2 failed, 2 passed."), "{summary}");
    assert!(summary.contains("Setup: 1 passed."), "{summary}");
    assert!(summary.contains("passed, before the PR: Same check before the PR"));

    // A crash leaves a running receipt; the next session marks it interrupted.
    let receipt = root.join("reviews/review1/experiments/0006.json");
    std::fs::write(
        &receipt,
        json!({"id":"6","kind":"test","revision":"head","status":"running","purpose":"Lost"})
            .to_string(),
    )
    .unwrap();
    let resumed = open(root, &podman, &source);
    let info = call(&resumed, "runtime_info", json!({})).await;
    assert_eq!(info["experiments"][5]["status"], "interrupted");
    assert_eq!(info["attempts"]["remaining"], 0);

    // A finished review releases its prepared workspaces.
    crow::execution::release_environments(&root.join("reviews/review1")).unwrap();
    assert!(
        !root
            .join("reviews/review1/experiments/environments")
            .exists()
    );
}

#[tokio::test]
async fn mcp_helper_uses_the_workers_podman_and_refuses_a_second_experiment() {
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = source(root);
    let podman = fake_podman(root);
    std::fs::write(root.join("podman/image"), "").unwrap();
    // The worker resolves the policy in its own environment.
    let worker = json!({"execution": {
        "podman": podman,
        "repositories": {"owner/repo": {"timeoutSeconds": 30, "maxRuns": 5}},
    }});
    let policy = crow::execution::resolve(&worker, "owner/repo", true)
        .unwrap()
        .unwrap();
    let context = json!({
        "root": root,
        "job": {"id": "review1", "settings": {"execution": policy}},
        "source": source,
    });
    let (source_file, context_file) = (root.join("source.json"), root.join("context.json"));
    std::fs::write(&source_file, source.to_string()).unwrap();
    std::fs::write(&context_file, context.to_string()).unwrap();
    // Providers start the helper with Crow's isolated environment, as here.
    let isolated = root.join("provider-home");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crow"))
        .arg("_inspection-mcp")
        .arg(&source_file)
        .arg(&context_file)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &isolated)
        .env("XDG_DATA_HOME", isolated.join(".local/share"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let tool = |id: u64, name: &str, arguments: Value| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    let experiment =
        |command: String| json!({"revision":"head","command":command,"purpose":"Probe"});
    // The first experiment runs until the test creates `release`.
    let release = root.join("release");
    let waiting = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done",
        release.display()
    );
    let test = async {
        for request in [
            tool(1, "run_experiment", experiment(waiting)),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
            tool(3, "run_experiment", experiment("true".into())),
        ] {
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        let mut early = BTreeMap::new();
        while early.len() < 2 {
            let line = output.next_line().await.unwrap().unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            early.insert(response["id"].as_u64().unwrap(), response);
        }
        // The ping and the refusal arrive while the first experiment is still running.
        assert_eq!(early[&2]["result"], json!({}));
        assert_eq!(early[&3]["result"]["isError"], true);
        let refusal = early[&3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(refusal.contains("still running"), "{refusal}");
        std::fs::write(&release, "").unwrap();
        let first: Value =
            serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], 1);
        assert!(
            first["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("passed")
        );
        // The refused call did not use an attempt.
        let info = tool(4, "runtime_info", json!({}));
        input
            .write_all(format!("{info}\n").as_bytes())
            .await
            .unwrap();
        let info: Value =
            serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
        let info: Value =
            serde_json::from_str(info["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(info["attempts"]["used"], 1);
        // Podman ran in the worker's environment, not the helper's.
        let homes = std::fs::read_to_string(root.join("podman/homes")).unwrap();
        let worker_home = std::env::var("HOME").unwrap();
        assert!(homes.lines().all(|home| home == worker_home), "{homes}");
    };
    tokio::time::timeout(Duration::from_secs(30), test)
        .await
        .unwrap();
}
