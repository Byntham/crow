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

#[tokio::test]
#[ignore = "requires rootless Podman and network access to build the runtime image"]
async fn real_podman_isolates_experiments() {
    let env = crow::execution::podman_env();
    if !crow::execution::image::exists("podman", &env)
        .await
        .unwrap()
    {
        crow::execution::image::build("podman", &env).await.unwrap();
    }
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
