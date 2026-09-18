//! Real container tests are opt-in and fail if their runtime is unavailable.
//! CROW_TEST_PODMAN=/path/to/podman CROW_TEST_IMAGE=sha256:... \
//! cargo test --test execution_mcp -- --ignored --nocapture
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
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
struct Mcp {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}
impl Mcp {
    fn new(source: &Path, context: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crow"))
            .args(["_inspection-mcp"])
            .arg(source)
            .arg(context)
            .env("GITHUB_TOKEN", "host-credential-canary")
            .env("CROW_CODEX_PROXY_TOKEN", "provider-credential-canary")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
        }
    }
    async fn request(&mut self, request: Value) -> Value {
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(Duration::from_secs(40), self.output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn call(&mut self, name: &str, args: Value) -> Value {
        let response = self
            .request(json!({"id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
            .await;
        assert_ne!(response["result"]["isError"], true, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    async fn close(mut self) {
        self.input.shutdown().await.unwrap();
        drop(self.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
#[ignore = "requires local rootless Podman and a preloaded Alpine image"]
async fn real_mcp_regression_isolation_deadline_recovery_and_publication() {
    let podman = std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into());
    let image = std::env::var("CROW_TEST_IMAGE")
        .expect("Set CROW_TEST_IMAGE to the local Alpine sha256 image ID");
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(
        repo.join("calculate.sh"),
        "total() { echo $(($1 + $2)); }\n",
    )
    .unwrap();
    // Archives must include ignored files and preserve literal substitution text.
    std::fs::write(
        repo.join(".gitattributes"),
        "calculate.sh export-ignore\nsubstitution.txt export-subst\n",
    )
    .unwrap();
    std::fs::write(repo.join("substitution.txt"), "$Format:%H$\n").unwrap();
    std::fs::write(repo.join("executable.sh"), "#!/bin/sh\necho executable\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        repo.join("executable.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("calculate.sh", repo.join("linked.sh")).unwrap();
    let canary = dir.path().join("host-secret");
    std::fs::write(&canary, "secret").unwrap();
    std::os::unix::fs::symlink(&canary, repo.join("host-link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("calculate.sh"),
        "total() { echo $(($1 - $2)); }\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "regression"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let bare = dir.path().join("bare.git");
    git(
        &repo,
        &[
            "clone",
            "--bare",
            repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let source = dir.path().join("source.json");
    let context = dir.path().join("context.json");
    let source_value = json!({"dir":bare,"head":head,"base":base,"target":"main","targetSha":base});
    std::fs::write(&source, source_value.to_string()).unwrap();
    // Container start, archive restore and exec are separate bounded operations.
    // Leave startup allowance while still forcing the 60-second command to time out.
    let config = json!({"podman":podman,"repositories":{"owner/repo":{"image":image,"timeoutSeconds":8,"memoryMiB":128,"workspaceMiB":32,"pids":32,"cpus":1,"maxRuns":10}}});
    std::fs::write(
        &context,
        json!({"source":source_value,"job":{"repo":"owner/repo","settings":{"execution":config}}})
            .to_string(),
    )
    .unwrap();
    let mut mcp = Mcp::new(&source, &context);
    let tools = mcp.request(json!({"id":1,"method":"tools/list"})).await;
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "run_experiment")
    );
    let command = ". ./calculate.sh; actual=$(total 2 3); echo expected=5 actual=$actual; test \"$actual\" = 5";
    let before = mcp
        .call(
            "run_experiment",
            json!({"revision":"base","command":command}),
        )
        .await;
    assert_eq!(before["status"], "passed", "{before}");
    assert_eq!(before["commit"], base);
    let after = mcp
        .call(
            "run_experiment",
            json!({"revision":"head","command":command}),
        )
        .await;
    assert_eq!(after["status"], "failed", "{after}");
    assert_eq!(after["exitCode"], 1);
    assert_eq!(after["commit"], head);
    let command = format!(
        r#"set -eu
[ ! -e '{canary}' ]; [ ! -e host-link ]; [ ! -e .git ]
[ -z "${{GITHUB_TOKEN:-}}" ]; [ -z "${{CROW_CODEX_PROXY_TOKEN:-}}" ]
[ -L linked.sh ]; ./executable.sh
[ "$(cat substitution.txt)" = '$Format:%H$' ]
! touch /etc/host-write 2>/dev/null
[ "$(ls /sys/class/net)" = lo ]
[ "$(awk '/CapEff/ {{ print $2 }}' /proc/self/status)" = 0000000000000000 ]
[ "$(awk '/NoNewPrivs/ {{ print $2 }}' /proc/self/status)" = 1 ]
[ "$(awk '/Seccomp:/ {{ print $2 }}' /proc/self/status)" = 2 ]
[ "$(cat /sys/fs/cgroup/memory.max)" = 134217728 ]
[ "$(cat /sys/fs/cgroup/pids.max)" = 32 ]
echo modified > calculate.sh
mkdir -p /tmp/www
cat > /tmp/www/server.sh <<'SERVER'
#!/bin/sh
read request
printf 'HTTP/1.0 200 OK\r\nContent-Length: 8\r\n\r\nhealthy\n'
SERVER
chmod +x /tmp/www/server.sh
nc -l -p 8080 -e /tmp/www/server.sh &
for attempt in 1 2 3 4 5; do
    wget -qO- http://127.0.0.1:8080 && exit 0
    sleep 0.1
done
exit 1
"#,
        canary = canary.display()
    );
    let isolation = mcp
        .call(
            "run_experiment",
            json!({"revision":"head","command":command}),
        )
        .await;
    assert_eq!(isolation["status"], "passed", "{isolation}");
    assert!(isolation["stdout"].as_str().unwrap().contains("healthy"));
    let fresh = mcp.call("run_experiment", json!({"revision":"head","command":"grep 'total()' calculate.sh; test ! -e /tmp/www; ! wget -qO- -T 1 http://127.0.0.1:8080"})).await;
    assert_eq!(fresh["status"], "passed", "{fresh}");
    let limit = mcp.call("run_experiment", json!({"revision":"head","command":"yes output | head -c 100000; echo error-output >&2; exit 7"})).await;
    assert_eq!(limit["exitCode"], 7, "{limit}");
    assert_eq!(limit["outputTruncated"], true);
    assert_eq!(limit["stdout"].as_str().unwrap().len(), 32768);
    let timeout = mcp
        .call(
            "run_experiment",
            json!({"revision":"head","command":"echo starting; sleep 60 & wait"}),
        )
        .await;
    assert_eq!(timeout["status"], "timed_out", "{timeout}");
    assert!(timeout["stdout"].as_str().unwrap().contains("starting"));
    let missing = mcp
        .call(
            "run_experiment",
            json!({"revision":"head","command":"this_tool_is_not_installed"}),
        )
        .await;
    assert_eq!(missing["status"], "error", "{missing}");
    mcp.close().await;
    let mut mcp = Mcp::new(&source, &context);
    let saved = mcp.call("list_experiments", json!({})).await;
    assert_eq!(saved["runs"].as_array().unwrap().len(), 7);
    assert!(
        saved["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == after["id"])
    );
    // Signal while a container and its descendant are active.
    mcp.input.write_all(format!("{}\n", json!({"id":1,"method":"tools/call","params":{"name":"run_experiment","arguments":{"revision":"head","command":"echo active; sleep 60 & wait"}}})).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        unsafe { libc::kill(mcp.child.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), mcp.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut mcp = Mcp::new(&source, &context);
    let saved = mcp.call("list_experiments", json!({})).await;
    assert!(
        saved["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "interrupted")
    );
    mcp.close().await;
    // Abrupt loss must recover a running receipt and remove its container.
    let mut mcp = Mcp::new(&source, &context);
    mcp.input.write_all(format!("{}\n", json!({"id":1,"method":"tools/call","params":{"name":"run_experiment","arguments":{"revision":"head","command":"sleep 60"}}})).as_bytes()).await.unwrap();
    let active = async {
        loop {
            let out = Command::new(&podman)
                .args([
                    "ps",
                    "--format={{.Names}}",
                    "--filter",
                    "name=crow-experiment-",
                ])
                .output()
                .unwrap();
            if !out.stdout.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), active)
        .await
        .unwrap();
    mcp.child.kill().await.unwrap();
    mcp.child.wait().await.unwrap();
    let mut mcp = Mcp::new(&source, &context);
    let recovered = mcp.call("list_experiments", json!({})).await;
    assert_eq!(recovered["runs"].as_array().unwrap().len(), 9);
    assert_eq!(
        recovered["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["status"] == "interrupted")
            .count(),
        2
    );
    let last = mcp
        .call(
            "run_experiment",
            json!({"revision":"base","command":"true"}),
        )
        .await;
    assert_eq!(last["status"], "passed", "{last}");
    mcp.close().await;
    let mut mcp = Mcp::new(&source, &context);
    let denied = mcp.request(json!({"id":1,"method":"tools/call","params":{"name":"run_experiment","arguments":{"revision":"head","command":"true"}}})).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied.to_string().contains("budget"));
    mcp.close().await;
    let mut report = json!({"summary":"The changed calculation fails the reproduction; the base passes.","findings":[{"title":"Restore addition in total","body":"The command produces -1 on the PR head and 5 on the base for total 2 3. Subtraction breaks callers expecting a sum.","path":"calculate.sh","line":1,"severity":"medium"}]});
    crow::execution::append_summary(&mut report, &dir.path().join("experiments")).unwrap();
    let report = crow::report::validate_report(&report).unwrap();
    let body = crow::report::report_body(
        &json!({"id":"fixture","repo":"owner/repo","comparison":source_value,"report":report}),
        &[],
        &[],
    )
    .unwrap();
    assert!(
        body.contains("Runtime experiments")
            && body.contains("passed")
            && body.contains("failed")
            && body.contains("timed_out"),
        "{body}"
    );
    let output = Command::new(&podman)
        .args([
            "ps",
            "--all",
            "--format={{.Names}}",
            "--filter",
            "name=crow-experiment-",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "Leaked containers: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(std::fs::read_to_string(canary).unwrap(), "secret");
    println!(
        "Real MCP/container regression, HTTP smoke test, isolation, output bounds, timeout, signal cleanup, resume and rendered report passed.\n{body}"
    );
}
