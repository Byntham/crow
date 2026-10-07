//! Real rootless Podman isolation checks. They build Crow's runtime image and
//! download one npm package, so they are ignored by default:
//!
//! ```sh
//! cargo test --test runtime_podman -- --ignored --test-threads=1
//! ```
use crow::execution::Execution;
use serde_json::{Value, json};
use std::{path::Path, process::Command};

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

fn source(root: &Path) -> Value {
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(
        work.join("package.json"),
        r#"{"name":"fixture","private":true}"#,
    )
    .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    std::fs::write(
        work.join("check.js"),
        "console.log(require('is-number')(5))\n",
    )
    .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "head"]);
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

/// Tries each way to create a user namespace, then starts an ordinary thread.
const NAMESPACE_PROBE: &str = r#"python3 - <<'EOF'
import ctypes, errno, os, platform, threading
libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
try:
    os.unshare(CLONE_NEWUSER)
    print("unshare allowed")
except OSError:
    print("unshare blocked")
clone = {"x86_64": 56, "aarch64": 220}[platform.machine()]
pid = libc.syscall(clone, CLONE_NEWUSER | 17, 0, 0, 0, 0)
if pid == 0:
    os._exit(0)
print("clone allowed" if pid > 0 else "clone blocked")
libc.syscall(435, 0, 0)
print("clone3 blocked" if ctypes.get_errno() == errno.ENOSYS else "clone3 allowed")
thread = threading.Thread(target=print, args=("thread ok",))
thread.start()
thread.join()
EOF"#;

async fn run(execution: &Execution, tool: &str, revision: &str, command: &str) -> Value {
    let args = json!({"revision": revision, "command": command, "purpose": command});
    execution.call(tool, &args).await.unwrap()
}

async fn ensure_image() {
    let env = crow::execution::podman_env();
    if !crow::execution::image::exists("podman", &env)
        .await
        .unwrap()
    {
        let cancel = tokio_util::sync::CancellationToken::new();
        crow::execution::image::build("podman", &env, &cancel)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires rootless Podman and network access to build the runtime image"]
async fn real_podman_isolates_experiments() {
    ensure_image().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let context = json!({
        "root": root,
        "job": {"id": "podmantest", "settings": {"execution": {
            "podman": "podman",
            "limits": {"timeoutSeconds": 120, "maxRuns": 12},
        }}},
        "source": source(root),
    });
    let execution = Execution::from_context(&context).unwrap().unwrap();
    let passed = |result: &Value| result["status"] == "passed";

    // The container user is not root, holds no capabilities and cannot gain them.
    let identity = run(
        &execution,
        "run_experiment",
        "head",
        "id -u; grep -E '^(CapEff|NoNewPrivs)' /proc/self/status",
    )
    .await;
    assert!(passed(&identity), "{identity}");
    let output = identity["stdout"].as_str().unwrap();
    assert!(output.starts_with("1000\n"), "{output}");
    assert!(output.contains("CapEff:\t0000000000000000"), "{output}");
    assert!(output.contains("NoNewPrivs:\t1"), "{output}");

    // Code cannot create a user namespace, the usual route to kernel privilege
    // escalation, but ordinary threads still work.
    let args = json!({"revision": "head", "command": NAMESPACE_PROBE, "purpose": "namespaces"});
    let namespaces = execution.call("run_experiment", &args).await.unwrap();
    assert!(passed(&namespaces), "{namespaces}");
    assert_eq!(
        namespaces["stdout"],
        "unshare blocked\nclone blocked\nclone3 blocked\nthread ok\n"
    );

    // The root filesystem is read-only and tests have no network.
    for probe in [
        "touch /etc/crow-probe",
        "curl -sS --max-time 10 https://registry.npmjs.org/",
    ] {
        let blocked = run(&execution, "run_experiment", "head", probe).await;
        assert_eq!(blocked["status"], "failed", "{probe}: {blocked}");
    }

    // Setup reaches allowed registries only, through the gateway.
    let other = run(
        &execution,
        "prepare_environment",
        "head",
        "curl -sS --max-time 10 https://example.com/",
    )
    .await;
    assert!(!passed(&other), "{other}");
    let setup = run(
        &execution,
        "prepare_environment",
        "head",
        "npm install --no-audit --no-fund is-number@7.0.0",
    )
    .await;
    assert!(passed(&setup), "{setup}");

    // The prepared dependency works offline, and the pinned source is restored over it.
    let check = run(&execution, "run_experiment", "head", "node check.js").await;
    assert!(passed(&check), "{check}");
    assert_eq!(check["stdout"], "true\n");
    let base = run(&execution, "run_experiment", "base", "test ! -f check.js").await;
    assert!(passed(&base), "{base}");

    // Containers are gone once each experiment finishes.
    let listed = Command::new("podman")
        .args([
            "ps",
            "--all",
            "--quiet",
            "--filter=label=crow.review=podmantest",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).trim().is_empty());
}

#[tokio::test]
#[ignore = "requires rootless Podman and network access to build the runtime image"]
async fn real_podman_runs_through_the_helper_a_provider_starts() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    ensure_image().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = source(root);
    let worker = json!({"execution": {"repositories": {"owner/repo": {"timeoutSeconds": 120}}}});
    let policy = crow::execution::resolve(&worker, "owner/repo", true)
        .unwrap()
        .unwrap();
    let context = json!({
        "root": root,
        "job": {"id": "podmanhelper", "settings": {"execution": policy}},
        "source": source,
    });
    let (source_file, context_file) = (root.join("source.json"), root.join("context.json"));
    std::fs::write(&source_file, source.to_string()).unwrap();
    std::fs::write(&context_file, context.to_string()).unwrap();
    // The same isolated environment Crow gives Claude Code and Codex, which
    // pass it on to the helper: another HOME, no runtime directory or D-Bus.
    let home = root.join("provider-home");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crow"))
        .arg("_inspection-mcp")
        .arg(&source_file)
        .arg(&context_file)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut call = async |name: &str, arguments: Value| -> Value {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}});
        input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let response: Value =
            serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap_or_else(|_| panic!("{text}"))
    };
    let info = call("runtime_info", json!({})).await;
    assert_eq!(info["imageReady"], true, "{info}");
    let ran = call(
        "run_experiment",
        json!({"revision": "head", "command": "test -f check.js", "purpose": "helper"}),
    )
    .await;
    assert_eq!(ran["status"], "passed", "{ran}");
}
