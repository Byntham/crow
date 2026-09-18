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
    std::fs::write(repo.join("test.cjs"),"const assert=require('node:assert/strict'); const isNumber=require('is-number'); assert(isNumber('42')); assert.equal(require('./calculate.cjs')(2,3),5); console.log('node regression test passed');\n").unwrap();
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
        json!({"revision":"head","setup":"echo deliberate-environment-failure >&2; exit 1"}),
    )
    .await;
    assert_eq!(broken["status"], "failed", "{broken}");
    assert_eq!(broken["phase"], "setup");
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
    let command = "test ! -e /run/crow-downloads/socket && test -z \"${HTTPS_PROXY:-}\" && .venv/bin/python -c 'import packaging; print(packaging.__version__)' && npm test";
    for (rev, environment, status) in [("base", &before, "passed"), ("head", &after, "failed")] {
        let result = call(
            &exec,
            "run_experiment",
            json!({"revision":rev,"environment":environment["id"],"command":command}),
        )
        .await;
        assert_eq!(result["status"], status, "{result}");
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
        json!({"setupBase":before,"setupHead":after,"cached":cached,"screenshot":shot}).to_string(),
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
    std::fs::write(repo.join("Cargo.toml"),"[package]\nname='crow-runtime-probe'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nitoa='=1.0.15'\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "#[test] fn formats_number() { assert_eq!(itoa::Buffer::new().format(42), \"42\"); }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("go.mod"),
        "module example.invalid/crowprobe\n\ngo 1.23\n\nrequire github.com/google/uuid v1.6.0\n",
    )
    .unwrap();
    std::fs::write(repo.join("probe_test.go"),"package crowprobe\nimport (\"testing\";\"github.com/google/uuid\")\nfunc TestUUID(t *testing.T) { if uuid.Nil.String() != \"00000000-0000-0000-0000-000000000000\" {t.Fatal(\"bad UUID\")} }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "toolchains"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":root,"source":{"dir":repo,"head":commit,"base":commit},"job":{"repo":"fixture/toolchains","settings":{"execution":{"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true}}}});
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
    let tested=call(&exec,"run_experiment",json!({"revision":"head","environment":setup["id"],"command":"cargo test --offline && GOPROXY=off go test ./..."})).await;
    assert_eq!(tested["status"], "passed", "{tested}");
}
