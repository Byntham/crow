//! Failure routing without running repository commands on the host.
use crow::execution::Execution;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn missing_runtime_stale_source_and_container_start_have_distinct_receipts() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("test.txt"), "source\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let runtime = temp.path().join("runtime");
    // This fake implements only infrastructure calls; it must never execute the command.
    std::fs::write(&runtime, "#!/bin/sh\ncase \"$1\" in\ninfo) printf '%s\\n' '{\"host\":{\"security\":{\"rootless\":true,\"seccompEnabled\":true},\"serviceIsRemote\":false,\"cgroupVersion\":\"v2\"}}';;\nrm) echo cleanup-was-called >&2; exit 125;;\nrun) echo 'fixture container startup refused' >&2; exit 125;;\n*) echo unexpected-operation >&2; exit 99;;\nesac\n").unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (name, executable, revision, expected) in [
        (
            "missing",
            temp.path().join("absent"),
            commit.clone(),
            "runtime_check",
        ),
        ("stale", runtime.clone(), "a".repeat(40), "source_archive"),
        ("start", runtime.clone(), commit.clone(), "container_start"),
    ] {
        let experiments = temp.path().join(name);
        let context = json!({"root":temp.path(), "source":{"dir":repo,"head":revision,"base":revision},"job":{"repo":"owner/repo","settings":{"execution":{"podman":executable,"repositories":{"owner/repo":{"image":format!("sha256:{}","b".repeat(64))}}}}}});
        let execution = Execution::from_context(&context, &experiments)
            .unwrap()
            .unwrap();
        let result = execution
            .call(
                "run_experiment",
                &json!({"revision":"head","command":"echo must-never-run"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "error", "{result}");
        assert_eq!(result["failureStage"], expected, "{result}");
        assert_eq!(result["containerStarted"], expected == "container_start");
        assert_eq!(
            result.get("cleanupError").is_some(),
            expected == "container_start"
        );
        assert!(result["error"].is_string(), "{result}");
        let persisted: Value = serde_json::from_slice(
            &std::fs::read(experiments.join(format!("{}.json", result["id"].as_str().unwrap())))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["failureStage"], expected);
        assert!(
            !result["stdout"]
                .as_str()
                .unwrap()
                .contains("must-never-run")
        );
    }
}

struct PostCommandFixture {
    _root: tempfile::TempDir,
    execution: Execution,
    experiments: std::path::PathBuf,
    canary: std::path::PathBuf,
}
impl PostCommandFixture {
    fn new(mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        // A tracked npm lock enables the optional package-download export stage.
        std::fs::write(repo.join("package-lock.json"), "{}\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "fixture"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let png_path = root.path().join("fixture.png");
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[1, 2, 3])
                .unwrap();
        }
        std::fs::write(&png_path, bytes).unwrap();
        let runtime = root.path().join("runtime");
        // Infrastructure-only simulator. It never evaluates shell commands, Python
        // helper arguments, or repository files. Sleep runs in this direct child,
        // so kill_on_drop can terminate it without leaving descendant processes.
        let script = format!(
            r#"#!/usr/bin/python3
import json, sys, time
mode = {mode}
png_path = {png_path}
args = sys.argv[1:]
if args[0] == 'info':
    print(json.dumps({{'host': {{'security': {{'rootless': True, 'seccompEnabled': True}}, 'serviceIsRemote': False, 'cgroupVersion': 'v2'}}}}))
    sys.exit(0)
if args[0] == 'run':
    mounts = [arg for arg in args if arg.startswith('--volume=')]
    assert all(arg.endswith(':/run/crow-downloads:ro,Z') for arg in mounts), mounts
    assert '--security-opt=label=disable' not in args, args
    sys.exit(0)
if args[0] == 'rm':
    sys.exit(0)
assert args[0] == 'exec', args
if args[1] == '--interactive':
    sys.stdin.buffer.read()
    sys.exit(0)
if args[2] == 'tar':
    if mode == 'snapshot_timeout':
        sys.stdout.buffer.write(b'incomplete snapshot')
        sys.stdout.buffer.flush()
        time.sleep(60)
    sys.exit(0)
if args[2] == 'python3':
    assert args[-2] == 'export', args
    assert mode == 'cache_timeout', mode
    time.sleep(60)
    sys.exit(0)
if args[2] == 'cat':
    if args[-1] == '/first.png':
        with open(png_path, 'rb') as image:
            sys.stdout.buffer.write(image.read())
        sys.exit(0)
    time.sleep(60)
    sys.exit(0)
assert args[2:4] == ['/bin/sh', '-c'], args
if args[4].startswith('# Crow package gateway preflight'):
    if mode == 'gateway_denied':
        print('Crow package gateway socket access denied. Check host directory permissions and SELinux policy.', file=sys.stderr)
        sys.exit(125)
    sys.exit(0)
assert mode != 'gateway_denied', 'Setup must not run after gateway preflight failure'
if args[4].startswith('read memory '):
    sys.exit(0)
print('fixture command completed', flush=True)
sys.exit(7 if mode in ['artifact_timeout', 'setup_failed'] else 0)
"#,
            mode = serde_json::to_string(mode).unwrap(),
            png_path = serde_json::to_string(&png_path).unwrap()
        );
        std::fs::write(&runtime, script).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        let experiments = root.path().join("experiments");
        let context = json!({"root":root.path(),"source":{"dir":repo,"head":commit,"base":commit},"job":{"repo":"owner/repo","number":42,"settings":{"execution":{"podman":runtime,"repositories":{"owner/repo":{"image":format!("sha256:{}","b".repeat(64)),"timeoutSeconds":3}}}}}});
        let execution = Execution::from_context(&context, &experiments)
            .unwrap()
            .unwrap();
        let canary = root.path().join("repository-command-was-executed");
        Self {
            _root: root,
            execution,
            experiments,
            canary,
        }
    }
    fn command(&self) -> String {
        format!("touch '{}'", self.canary.display())
    }
    fn snapshot(&self, result: &Value) -> std::path::PathBuf {
        self.experiments
            .join("environments")
            .join(format!("{}.tar", result["id"].as_str().unwrap()))
    }
    fn check_clean(&self, result: &Value) {
        assert!(
            !self.canary.exists(),
            "The simulator evaluated a repository command"
        );
        assert!(result.get("cleanupError").is_none(), "{result}");
        for dir in [
            &self.experiments,
            &self.experiments.join("artifacts"),
            &self.experiments.join("environments"),
        ] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries {
                    let entry = entry.unwrap();
                    assert!(
                        !entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".crow-runtime-"),
                        "Temporary export survived: {}",
                        entry.path().display()
                    );
                }
            }
        }
        let persisted: Value = serde_json::from_slice(
            &std::fs::read(
                self.experiments
                    .join(format!("{}.json", result["id"].as_str().unwrap())),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted, *result);
    }
}

#[tokio::test]
async fn denied_gateway_is_reported_before_repository_setup() {
    let fixture = PostCommandFixture::new("gateway_denied");
    let result = fixture
        .execution
        .call(
            "prepare_environment",
            &json!({"revision":"head","setup":fixture.command()}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "error", "{result}");
    assert_eq!(result["failureStage"], "gateway_start", "{result}");
    assert_eq!(result["containerStarted"], true);
    assert!(result["error"].as_str().unwrap().contains("SELinux policy"));
    assert!(!fixture.snapshot(&result).exists());
    fixture.check_clean(&result);
}

#[tokio::test]
async fn optional_cache_timeout_preserves_completed_setup_and_required_snapshot() {
    let fixture = PostCommandFixture::new("cache_timeout");
    let result = fixture
        .execution
        .call(
            "prepare_environment",
            &json!({"revision":"head","setup":fixture.command()}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(result["exitCode"], 0);
    assert!(
        result["cacheSaveError"]
            .as_str()
            .unwrap()
            .contains("did not finish")
    );
    assert!(result.get("failureStage").is_none(), "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("fixture command completed")
    );
    assert!(fixture.snapshot(&result).is_file());
    fixture.check_clean(&result);
}

#[tokio::test]
async fn screenshot_timeout_preserves_command_failure_and_already_saved_image() {
    let fixture = PostCommandFixture::new("artifact_timeout");
    let result = fixture.execution.call("run_experiment", &json!({"revision":"head","command":fixture.command(),"artifacts":["/first.png","/second.png","/third.png"]}), CancellationToken::new()).await.unwrap();
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["exitCode"], 7);
    assert_eq!(result["failureStage"], "test_command");
    assert_eq!(result["artifacts"].as_array().unwrap().len(), 3);
    assert_eq!(result["artifacts"][0]["saved"], true);
    for index in [1, 2] {
        assert_eq!(result["artifacts"][index]["saved"], false);
        assert!(
            result["artifacts"][index]["error"]
                .as_str()
                .unwrap()
                .contains("did not finish")
        );
    }
    let first = fixture
        .experiments
        .join("artifacts")
        .join(format!("{}-0.png", result["id"].as_str().unwrap()));
    assert!(first.is_file());
    let image = fixture
        .execution
        .call(
            "read_artifact",
            &json!({"experiment":result["id"],"index":0}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(image["content"][1]["type"], "image");
    fixture.check_clean(&result);
}

#[tokio::test]
async fn failed_setup_and_incomplete_required_snapshot_never_become_usable_environments() {
    for (mode, status, stage) in [
        ("setup_failed", "failed", "setup_command"),
        ("snapshot_timeout", "timed_out", "snapshot_export"),
    ] {
        let fixture = PostCommandFixture::new(mode);
        let result = fixture
            .execution
            .call(
                "prepare_environment",
                &json!({"revision":"head","setup":fixture.command()}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], status, "{result}");
        assert_eq!(result["failureStage"], stage, "{result}");
        assert!(!fixture.snapshot(&result).exists());
        assert!(result.get("cacheSaveError").is_none());
        assert!(
            fixture
                .execution
                .call(
                    "run_experiment",
                    &json!({"revision":"head","command":"true","environment":result["id"]}),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        fixture.check_clean(&result);
    }
}

#[tokio::test]
async fn unavailable_artifact_directory_warns_without_relabeling_successful_test() {
    let fixture = PostCommandFixture::new("artifact_directory");
    std::fs::write(fixture.experiments.join("artifacts"), b"not a directory").unwrap();
    let result = fixture
        .execution
        .call(
            "run_experiment",
            &json!({"revision":"head","command":fixture.command(),"artifacts":["/first.png"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(result["artifacts"][0]["saved"], false);
    assert!(result["artifacts"][0]["error"].is_string());
    fixture.check_clean(&result);
}
