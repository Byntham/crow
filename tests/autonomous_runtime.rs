//! Real managed setup, package gateway, offline tests and image evidence.
//! CROW_TEST_PODMAN=/path/to/podman cargo test --test autonomous_runtime -- --ignored --nocapture
use crow::execution::Execution;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use tokio_util::sync::CancellationToken;
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
async fn call(exec: &Execution, name: &str, args: Value) -> Value {
    let result = exec
        .call(name, &args, CancellationToken::new())
        .await
        .unwrap();
    println!(
        "{name}: {}",
        serde_json::to_string(&result)
            .unwrap()
            .chars()
            .take(1800)
            .collect::<String>()
    );
    result
}

#[tokio::test]
#[ignore = "requires rootless Podman and public npm registry access"]
async fn managed_gateway_handles_concurrent_package_downloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("package.json"), "{\"private\":true}").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "download probe"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":root,"source":{"dir":repo,"base":commit,"head":commit},"job":{"repo":"fixture/downloads","settings":{"execution":{"automatic":true,"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into())}}}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let setup = r#"set -e
seq 1 24 | xargs -P24 -I{} sh -c 'curl --fail --silent --show-error --max-time 30 https://registry.npmjs.org/is-number/7.0.0 > /tmp/package-{}.json'
python3 - <<'PY'
import json
from pathlib import Path
files = list(Path('/tmp').glob('package-*.json'))
assert len(files) == 24
assert all(json.loads(p.read_text())['version'] == '7.0.0' for p in files)
print('24 concurrent package downloads verified')
PY"#;
    let result = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(result["status"], "passed", "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("24 concurrent package downloads verified")
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman; prepares a managed image and downloads public test dependencies"]
async fn managed_setup_downloads_recovery_cache_and_screenshots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("package.json"),r#"{"name":"crow-autonomous-fixture","version":"1.0.0","private":true,"scripts":{"test":"node test.cjs"},"dependencies":{"is-number":"7.0.0"}}"#).unwrap();
    std::fs::write(repo.join("test.cjs"),"const assert=require('node:assert/strict'); const isNumber=require('is-number'); assert(isNumber('42')); assert.equal(require('./calculate.cjs')(2,3),5,'APPLICATION_REGRESSION: calculate(2,3) must equal 5'); console.log('node regression test passed');\n").unwrap();
    std::fs::write(repo.join("calculate.cjs"), "module.exports=(a,b)=>a+b;\n").unwrap();
    std::fs::write(repo.join("requirements.txt"), "packaging==24.2\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("calculate.cjs"), "module.exports=(a,b)=>a-b;\n").unwrap();
    git(&repo, &["commit", "-am", "regression"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let source = json!({"dir":repo,"base":base,"head":head});
    let cfg = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true,"repositories":{"fixture/project":{"timeoutSeconds":120,"memoryMiB":1536,"workspaceMiB":512,"pids":256}}});
    let context = json!({"root":root,"job":{"repo":"fixture/project","settings":{"execution":cfg}},"source":source});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let discovery = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    assert_eq!(discovery["projects"].as_array().unwrap().len(), 2);
    let broken = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":"echo deliberate-environment-failure >&2; curl --fail --silent --show-error https://not-a-package-host.invalid/package; exit 1"}),
    )
    .await;
    assert_eq!(broken["status"], "failed", "{broken}");
    assert_eq!(broken["phase"], "setup");
    assert!(
        broken["gatewayErrors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|error| error
                .as_str()
                .is_some_and(|text| text.contains("Package host is not allowed")))),
        "{broken}"
    );
    let setup = "npm install --no-audit --no-fund && python3 -m venv /workspace/.venv && /workspace/.venv/bin/pip install -r requirements.txt";
    let before = call(
        &exec,
        "prepare_environment",
        json!({"revision":"base","setup":setup}),
    )
    .await;
    assert_eq!(before["status"], "passed", "{before}");
    let after = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(after["status"], "passed", "{after}");
    let cached = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(cached["cached"], true);
    assert!(
        exec.call(
            "run_experiment",
            &json!({"revision":"head","command":"npm test","environment":before["id"]}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let prerequisites = "test ! -e /run/crow-downloads/socket && test -z \"${HTTPS_PROXY:-}\" && .venv/bin/python -c 'import packaging; assert packaging.__version__ == \"24.2\"' && node -e 'require(\"node:assert/strict\")(require(\"is-number\")(\"42\"))' && echo RUNTIME_PREREQUISITES_PASSED";
    let mut prerequisites_results = Vec::new();
    let mut application_results = Vec::new();
    for (rev, environment, status) in [("base", &before, "passed"), ("head", &after, "failed")] {
        // An isolation or dependency failure must never satisfy the expected
        // application regression on head. Verify prerequisites independently.
        let checked = call(
            &exec,
            "run_experiment",
            json!({"revision":rev,"environment":environment["id"],"command":prerequisites}),
        )
        .await;
        assert_eq!(
            checked["status"], "passed",
            "{rev} prerequisites: {checked}"
        );
        assert_eq!(checked["exitCode"], 0, "{checked}");
        assert!(
            checked["stdout"]
                .as_str()
                .unwrap()
                .contains("RUNTIME_PREREQUISITES_PASSED"),
            "{checked}"
        );
        prerequisites_results.push(checked);
        let result = call(
            &exec,
            "run_experiment",
            json!({"revision":rev,"environment":environment["id"],"command":"npm test"}),
        )
        .await;
        assert_eq!(result["status"], status, "{result}");
        if rev == "base" {
            assert_eq!(result["exitCode"], 0, "{result}");
            assert!(
                result["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("node regression test passed"),
                "{result}"
            );
        } else {
            assert_eq!(result["exitCode"], 1, "{result}");
            let stderr = result["stderr"].as_str().unwrap();
            assert!(
                stderr.contains("APPLICATION_REGRESSION: calculate(2,3) must equal 5")
                    && stderr.contains("ERR_ASSERTION")
                    && stderr.contains("actual: -1")
                    && stderr.contains("expected: 5"),
                "The application assertion must cause the head failure: {result}"
            );
        }
        application_results.push(result);
    }
    let changed_source = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":"printf 'tampered by setup' > calculate.cjs"}),
    )
    .await;
    assert_eq!(changed_source["status"], "passed", "{changed_source}");
    let restored_source=call(&exec,"run_experiment",json!({"revision":"head","environment":changed_source["id"],"command":"test \"$(cat calculate.cjs)\" = 'module.exports=(a,b)=>a-b;'"})).await;
    assert_eq!(restored_source["status"], "passed", "{restored_source}");
    let command = r#"node --input-type=module <<'JS'
import { chromium } from '/opt/browser/node_modules/playwright-core/index.mjs';
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',args:['--no-sandbox','--disable-dev-shm-usage']});
const page=await browser.newPage({viewport:{width:640,height:320}});
await page.setContent('<html><body style="background:#123;color:white"><h1>Real Chromium screenshot</h1></body></html>');
// setContent can finish before Chromium's first frame is available for capture.
await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
await page.screenshot({path:'/tmp/evidence.png'});
await browser.close();
JS"#;
    let shot=call(&exec,"run_experiment",json!({"revision":"head","environment":after["id"],"command":command,"artifacts":["/tmp/evidence.png","/tmp/missing.png"]})).await;
    assert_eq!(shot["status"], "passed", "{shot}");
    assert_eq!(shot["artifacts"][0]["saved"], true);
    assert_eq!(shot["artifacts"][1]["saved"], false);
    let artifact = exec
        .call(
            "read_artifact",
            &json!({"experiment":shot["id"],"index":0}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(artifact["content"][1]["type"], "image");
    assert_eq!(artifact["content"][1]["mimeType"], "image/png");
    std::fs::copy(
        temp.path()
            .join("experiments/artifacts")
            .join(format!("{}-0.png", shot["id"].as_str().unwrap())),
        root.join("chromium.png"),
    )
    .unwrap();
    std::fs::write(
        root.join("result.json"),
        json!({"setupBase":before,"setupHead":after,"cached":cached,"runtimePrerequisites":prerequisites_results,"applicationTests":application_results,"screenshot":shot}).to_string(),
    )
    .unwrap();
    println!(
        "Managed dependency preparation, repair, exact-revision cache, offline base/head tests and PNG image delivery passed."
    );
}

#[test]
fn saved_screenshot_uses_native_mcp_image_content() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let experiments = dir.path().join("experiments");
    std::fs::create_dir_all(experiments.join("artifacts")).unwrap();
    let id = "a".repeat(32);
    let source = json!({"dir":dir.path(),"base":"b".repeat(40),"head":"c".repeat(40)});
    let context = json!({"source":source,"job":{"repo":"owner/repo","settings":{"execution":{"automatic":true}}}});
    std::fs::write(dir.path().join("source.json"), source.to_string()).unwrap();
    std::fs::write(dir.path().join("context.json"), context.to_string()).unwrap();
    std::fs::write(experiments.join(format!("{id}.json")),json!({"id":id,"status":"passed","commit":"c".repeat(40),"command":"capture screenshot","artifacts":[{"saved":true,"path":"/tmp/screenshot.png"}]}).to_string()).unwrap();
    let file = std::fs::File::create(experiments.join(format!("artifacts/{id}-0.png"))).unwrap();
    let mut encoder = png::Encoder::new(file, 1, 1);
    encoder.set_color(png::ColorType::Rgb);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[20, 80, 40])
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_crow"))
        .arg("_inspection-mcp")
        .arg(dir.path().join("source.json"))
        .arg(dir.path().join("context.json"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input,"{}",json!({"id":1,"method":"tools/call","params":{"name":"read_artifact","arguments":{"experiment":id,"index":0}}})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["result"]["content"][0]["type"], "text");
    assert_eq!(response["result"]["content"][1]["type"], "image");
    assert_eq!(response["result"]["content"][1]["mimeType"], "image/png");
    assert!(
        response["result"]["content"][1]["data"]
            .as_str()
            .unwrap()
            .starts_with("iVBOR")
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman and public Cargo/Go module downloads"]
async fn managed_rust_and_go_dependencies_run_offline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(&root).unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname='crow-runtime-probe'\nversion='0.1.0'\nedition='2024'\nrust-version='1.88'\n[dependencies]\nitoa='=1.0.15'\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        r#"fn format_positive(value: Option<i32>) -> Option<String> {
    if let Some(value) = value && value > 0 {
        Some(itoa::Buffer::new().format(value).to_owned())
    } else {
        None
    }
}
#[test]
fn let_chain_handles_present_absent_and_rejected_values() {
    assert_eq!(format_positive(Some(42)).as_deref(), Some("42"));
    assert_eq!(format_positive(Some(-1)), None);
    assert_eq!(format_positive(None), None);
}
"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("go.mod"),
        "module example.invalid/crowprobe\n\ngo 1.23\n\nrequire github.com/google/uuid v1.6.0\n",
    )
    .unwrap();
    std::fs::write(repo.join("probe_test.go"),"package crowprobe\nimport (\"testing\";\"github.com/google/uuid\")\nfunc TestUUID(t *testing.T) { if uuid.Nil.String() != \"00000000-0000-0000-0000-000000000000\" {t.Fatal(\"bad UUID\")} }\n").unwrap();
    std::fs::write(
        repo.join("Cargo.lock"),
        r#"version = 4
[[package]]
name = "crow-runtime-probe"
version = "0.1.0"
dependencies = ["itoa"]
[[package]]
name = "itoa"
version = "1.0.15"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "4a5f13b858c8d314ee3e8f639011f7ccefe71f97f96e50151fb991f267928e2c"
"#,
    )
    .unwrap();
    std::fs::write(repo.join("go.sum"), "github.com/google/uuid v1.6.0 h1:NIvaJDMOsjHA8n1jAhLSgzrAzy1Hgr+hNrb57e+94F0=\ngithub.com/google/uuid v1.6.0/go.mod h1:TIyPZe4MgqvfeYDBFedMoGGpEw/LqOeaOT+nhxU+yHo=\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "toolchains"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    // Reuse the toolchain image, but require actual downloads on this test's first
    // preparation rather than silently importing a previous test run's packages.
    let context = json!({"root":root,"source":{"dir":repo,"head":commit,"base":commit},"job":{"repo":format!("fixture/toolchains-{}",crow::util::id()),"settings":{"execution":{"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true}}}});
    let exec = Execution::from_context(&context, &dir.path().join("experiments"))
        .unwrap()
        .unwrap();
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":"cargo fetch && go mod download all"}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    assert!(setup["packageCacheKey"].is_null(), "{setup}");
    assert!(
        !setup["packageCacheRestored"].as_bool().unwrap_or(false),
        "{setup}"
    );
    let tested=call(&exec,"run_experiment",json!({"revision":"head","environment":setup["id"],"command":"rustc --version && cargo --version && node --version && python3 --version && node -e \"require('node:assert/strict').ok(Number(process.versions.node.split('.')[0]) >= 24)\" && cargo test --offline && GOPROXY=off go test ./..."})).await;
    assert_eq!(tested["status"], "passed", "{tested}");
    let output = tested["stdout"].as_str().unwrap();
    for marker in [
        "rustc ",
        "cargo ",
        "Python 3.",
        "let_chain_handles_present_absent_and_rejected_values",
    ] {
        assert!(
            output.contains(marker),
            "Missing toolchain evidence {marker}: {tested}"
        );
    }
    assert!(setup["cacheSaveError"].is_null(), "{setup}");
    let second = Execution::from_context(&context, &dir.path().join("next-review"))
        .unwrap()
        .unwrap();
    let fresh = call(&second,"prepare_environment",json!({"revision":"head","setup":"test ! -e .crow-home/.cargo && test ! -e .crow-home/go/pkg/mod/cache/download && echo 'NEXT_REVIEW_HAS_NO_CARGO_OR_GO_CACHE' && cargo fetch --locked && go mod download all"})).await;
    assert_eq!(fresh["status"], "passed", "{fresh}");
    assert!(fresh["packageCacheKey"].is_null(), "{fresh}");
    assert!(
        !fresh["packageCacheRestored"].as_bool().unwrap_or(false),
        "{fresh}"
    );
    assert!(fresh["packageCache"].is_null(), "{fresh}");
    assert!(
        fresh["stdout"]
            .as_str()
            .unwrap()
            .contains("NEXT_REVIEW_HAS_NO_CARGO_OR_GO_CACHE"),
        "{fresh}"
    );
    assert!(
        fresh["stderr"]
            .as_str()
            .unwrap()
            .contains("Downloaded itoa v1.0.15"),
        "Expected a fresh Cargo download: {fresh}"
    );
    let retested = call(&second,"run_experiment",json!({"revision":"head","environment":fresh["id"],"command":"cargo test --offline --locked && GOPROXY=off go test ./..."})).await;
    assert_eq!(retested["status"], "passed", "{retested}");
    assert!(
        retested["stdout"]
            .as_str()
            .unwrap()
            .contains("let_chain_handles_present_absent_and_rejected_values"),
        "{retested}"
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman and the managed Python image"]
async fn restore_ignores_repository_imports_and_user_site_hooks_and_workspaces_are_not_shared() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "original pinned source\n").unwrap();
    std::fs::write(
        repo.join("tarfile.py"),
        "print('REPOSITORY MODULE EXECUTED'); raise SystemExit(0)\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "hostile imports"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let mut context = json!({"root":temp.path(),"source":{"dir":repo,"base":commit,"head":commit},"job":{"repo":"fixture/restore","settings":{"execution":{"automatic":true,"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into())}}}});
    // Reuse the managed image from the suite, but keep dependency snapshots local to this test.
    // The managed image tag is deterministic and local to Podman.
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let setup = "printf 'tampered by setup\\n' > tracked.txt";
    let prepared = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(prepared["status"], "passed", "{prepared}");
    let result = call(&exec, "run_experiment", json!({"revision":"head","environment":prepared["id"],"command":"test \"$(cat tracked.txt)\" = 'original pinned source'"})).await;
    assert_eq!(result["status"], "passed", "{result}");
    let hook = r#"set -e
printf 'tampered by setup\n' > tracked.txt
rm tarfile.py
python3 -I - <<'PYTHON'
import site
from pathlib import Path
path = Path(site.getusersitepackages())
path.mkdir(parents=True, exist_ok=True)
(path / 'crow_attack.pth').write_text('import os; os._exit(0)\n')
PYTHON"#;
    let poisoned = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":hook}),
    )
    .await;
    assert_eq!(poisoned["status"], "passed", "{poisoned}");
    let result = call(&exec, "run_experiment", json!({"revision":"head","environment":poisoned["id"],"command":"test \"$(cat tracked.txt)\" = 'original pinned source' && test -f tarfile.py && test -n \"$(find .crow-home -name crow_attack.pth)\""})).await;
    assert_eq!(result["status"], "passed", "{result}");

    let checkout = temp.path().join("another-job-checkout");
    git(
        temp.path(),
        &["clone", repo.to_str().unwrap(), checkout.to_str().unwrap()],
    );
    context["source"]["dir"] = json!(checkout);
    let other = Execution::from_context(&context, &temp.path().join("other-experiments"))
        .unwrap()
        .unwrap();
    let cached = call(
        &other,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(cached["status"], "passed", "{cached}");
    assert_ne!(
        cached["cached"], true,
        "Whole workspaces must not cross review boundaries: {cached}"
    );
    context["job"]["repo"] = json!("fixture/different-repository");
    let different = Execution::from_context(&context, &temp.path().join("different-experiments"))
        .unwrap()
        .unwrap();
    let uncached = call(
        &different,
        "prepare_environment",
        json!({"revision":"head","setup":setup}),
    )
    .await;
    assert_eq!(uncached["status"], "passed", "{uncached}");
    assert_ne!(uncached["cached"], true, "{uncached}");
    let output = call(&exec, "run_experiment", json!({"revision":"head","command":"printf 'BEGIN\\n'; head -c 40000 /dev/zero | tr '\\000' '.'; printf '\\nFINAL_FAILURE_DIAGNOSIS\\n'; exit 1"})).await;
    assert_eq!(output["status"], "failed");
    assert_eq!(output["outputTruncated"], true);
    let stdout = output["stdout"].as_str().unwrap();
    assert!(
        stdout.starts_with("BEGIN")
            && stdout.contains("output omitted")
            && stdout.ends_with("FINAL_FAILURE_DIAGNOSIS\n")
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman and the public npm registry"]
async fn updated_pr_installs_fresh_dependencies_and_releases_finished_workspaces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("package.json"), r#"{"name":"cache-fixture","version":"1.0.0","private":true,"dependencies":{"is-number":"7.0.0"}}"#).unwrap();
    std::fs::write(repo.join("package-lock.json"), r#"{"name":"cache-fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"cache-fixture","version":"1.0.0","dependencies":{"is-number":"7.0.0"}},"node_modules/is-number":{"version":"7.0.0","resolved":"https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz","integrity":"sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng=="}}}"#).unwrap();
    std::fs::write(repo.join("value.txt"), "first commit\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "first revision"]);
    let first = git(&repo, &["rev-parse", "HEAD"]);
    let unique = temp
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .trim_start_matches('.');
    let mut context = json!({"root":root,"source":{"dir":repo,"head":first,"base":first},"job":{"id":"first","number":1,"repo":format!("fixture/cache-{unique}"),"settings":{"execution":{"automatic":true,"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into())}}}});
    let first_dir = temp.path().join("reviews/first/experiments");
    let exec = Execution::from_context(&context, &first_dir)
        .unwrap()
        .unwrap();
    let initial = call(&exec,"prepare_environment",json!({"revision":"head","setup":"npm ci --no-audit --no-fund && printf 'must not be shared' > generated-output.txt"})).await;
    assert_eq!(initial["status"], "passed", "{initial}");
    assert!(initial["cacheSaveError"].is_null(), "{initial}");
    assert_ne!(initial["packageCacheRestored"], true, "{initial}");
    std::fs::write(repo.join("value.txt"), "updated commit\n").unwrap();
    git(
        &repo,
        &["commit", "-am", "update source without dependency changes"],
    );
    let second = git(&repo, &["rev-parse", "HEAD"]);
    context["source"]["head"] = json!(second);
    context["job"]["id"] = json!("second");
    let second_dir = temp.path().join("reviews/second/experiments");
    let next = Execution::from_context(&context, &second_dir)
        .unwrap()
        .unwrap();
    let fresh = call(&next,"prepare_environment",json!({"revision":"head","setup":"test ! -e generated-output.txt && test ! -e node_modules && test ! -e .crow-home/.npm && test \"$(cat value.txt)\" = 'updated commit' && echo NEXT_REVIEW_STARTED_CLEAN && npm ci --no-audit --no-fund"})).await;
    assert_eq!(fresh["status"], "passed", "{fresh}");
    assert!(fresh["packageCacheKey"].is_null(), "{fresh}");
    assert_ne!(fresh["packageCacheRestored"], true, "{fresh}");
    assert!(
        fresh["stdout"]
            .as_str()
            .unwrap()
            .contains("NEXT_REVIEW_STARTED_CLEAN"),
        "{fresh}"
    );
    let tested=call(&next,"run_experiment",json!({"revision":"head","environment":fresh["id"],"command":"node -e \"if(!require('is-number')('42'))process.exit(1)\" && test \"$(cat value.txt)\" = 'updated commit'"})).await;
    assert_eq!(tested["status"], "passed", "{tested}");
    let cleanup = crow::retention::cleanup(
        temp.path(),
        &json!({"retentionDays":7}),
        &[
            json!({"id":"first","state":"completed","updatedAt":crow::util::now()}),
            json!({"id":"second","state":"paused","updatedAt":crow::util::now()}),
        ],
    )
    .unwrap();
    assert!(
        cleanup["warnings"].as_array().unwrap().is_empty(),
        "{cleanup}"
    );
    assert!(!first_dir.join("environments").exists());
    assert!(
        first_dir
            .join(format!("{}.json", initial["id"].as_str().unwrap()))
            .exists()
    );
    assert!(second_dir.join("environments").exists());
    let superseded = crow::retention::cleanup(
        temp.path(),
        &json!({"retentionDays":7}),
        &[json!({"id":"second","state":"superseded","updatedAt":crow::util::now()})],
    )
    .unwrap();
    assert!(
        superseded["warnings"].as_array().unwrap().is_empty(),
        "{superseded}"
    );
    assert!(!second_dir.join("environments").exists());
    assert!(
        second_dir
            .join(format!("{}.json", fresh["id"].as_str().unwrap()))
            .exists()
    );
    println!(
        "Updated source started without dependency caches or generated files, fetched npm dependencies, and passed offline tests. Cleanup preserved paused work, then released superseded snapshots while retaining receipts."
    );
}
