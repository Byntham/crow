//! Repository discovery and Crow-owned toolchain provisioning.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

pub fn fingerprint(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}
pub async fn discover(source: &Value, revision: &str) -> Result<Value> {
    let commit =
        crate::inspection::revision(source[revision].as_str().context("Missing revision")?)?;
    let dir = Path::new(source["dir"].as_str().context("Missing source directory")?);
    let files = crate::inspection::git(
        dir,
        &[
            "ls-tree".into(),
            "-r".into(),
            "--name-only".into(),
            "-z".into(),
            commit.into(),
        ],
    )
    .await?;
    let files: Vec<_> = files.split('\0').filter(|s| !s.is_empty()).collect();
    let mut projects = Vec::new();
    let mut evidence = Vec::new();
    for path in &files {
        let name = path.rsplit('/').next().unwrap_or(path);
        if path.starts_with(".github/workflows/")
            || [
                "Dockerfile",
                "compose.yaml",
                "docker-compose.yml",
                "Makefile",
                "README.md",
            ]
            .contains(&name)
        {
            evidence.push(*path);
        }
        let language = match name {
            "package.json" => "node",
            "pyproject.toml" | "requirements.txt" | "setup.py" => "python",
            "Cargo.toml" => "rust",
            "go.mod" => "go",
            _ => continue,
        };
        if projects.len() >= 100 {
            continue;
        }
        let directory = path.rsplit_once('/').map_or(".", |(d, _)| d);
        let has = |file: &str| {
            files.contains(
                &if directory == "." {
                    file.to_owned()
                } else {
                    format!("{directory}/{file}")
                }
                .as_str(),
            )
        };
        let blob = crate::inspection::read_blob(source, path, Some(commit)).await;
        let (setup, tests, start) = match language {
            "node" => {
                let package: Value = blob
                    .as_ref()
                    .ok()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                let setup = if has("pnpm-lock.yaml") {
                    "npm install --prefix /workspace/.crow-tools pnpm && /workspace/.crow-tools/node_modules/.bin/pnpm install --frozen-lockfile"
                } else if has("yarn.lock") {
                    "npm install --prefix /workspace/.crow-tools yarn && /workspace/.crow-tools/node_modules/.bin/yarn install --frozen-lockfile"
                } else if has("package-lock.json") {
                    "npm ci --no-audit --no-fund"
                } else {
                    "npm install --no-audit --no-fund"
                };
                (
                    setup,
                    if package["scripts"]["test"].is_string() {
                        "npm test"
                    } else {
                        ""
                    },
                    if package["scripts"]["start"].is_string() {
                        "npm start"
                    } else if package["scripts"]["dev"].is_string() {
                        "npm run dev"
                    } else {
                        ""
                    },
                )
            }
            "python" => (
                if has("requirements.txt") {
                    "python3 -m venv /workspace/.venv && /workspace/.venv/bin/pip install -r requirements.txt"
                } else {
                    "python3 -m venv /workspace/.venv && /workspace/.venv/bin/pip install -e ."
                },
                "/workspace/.venv/bin/python -m pytest",
                "",
            ),
            "rust" => ("cargo fetch --locked", "cargo test --offline --locked", ""),
            "go" => ("go mod download", "GOPROXY=off go test ./...", ""),
            _ => unreachable!(),
        };
        projects.push(json!({"directory":directory,"manifest":path,"language":language,"setup":setup,"test":tests,"start":start}));
    }
    evidence.truncate(100);
    Ok(
        json!({"revision":revision,"commit":commit,"projects":projects,"instructionsToInspect":evidence,"browser":{"module":"/opt/browser/node_modules/playwright-core/index.mjs","executable":"/usr/bin/chromium","args":["--no-sandbox","--disable-dev-shm-usage"]},"note":"These are setup candidates, not verified commands. Read CI and manifests, respect runtime versions, select affected projects, and repair failed setup using logs. Do not change application code to make a test pass. Static sites and standard-library projects need no dependency installation."}),
    )
}

pub async fn image(
    executable: &str,
    env: &BTreeMap<String, String>,
    cache: &Path,
    cancel: CancellationToken,
    seconds: u64,
) -> Result<String> {
    crate::util::private_dir(cache)?;
    let version = fingerprint(&[
        include_str!("runtime/Containerfile"),
        include_str!("runtime/proxy.py"),
    ]);
    let tag = format!("localhost/crow-runtime:{}", &version[..20]);
    let inspect = || async {
        crate::process::run(
            executable,
            &[
                "image".into(),
                "inspect".into(),
                "--format={{.Id}}".into(),
                tag.clone(),
            ],
            crate::process::RunOptions {
                env: Some(env.clone()),
                cancel: cancel.clone(),
                timeout: Some(Duration::from_secs(20)),
                ..Default::default()
            },
        )
        .await
    };
    if let Ok(output) = inspect().await {
        return image_id(&output.stdout);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join("image.lock"))?;
    // Image builds are shared between repositories. Wait without blocking the executor.
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Image preparation interrupted"), _ = tokio::time::sleep(Duration::from_millis(200))=>{}}
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Ok(output) = inspect().await {
        return image_id(&output.stdout);
    }
    let context = tempfile::tempdir_in(cache)?;
    std::fs::write(
        context.path().join("Containerfile"),
        include_str!("runtime/Containerfile"),
    )?;
    std::fs::write(
        context.path().join("proxy.py"),
        include_str!("runtime/proxy.py"),
    )?;
    // Only Crow-owned files enter this build. Never execute repository Dockerfiles here.
    crate::process::run(
        executable,
        &[
            "build".into(),
            "--force-rm".into(),
            "--tag".into(),
            tag.clone(),
            "--file".into(),
            context
                .path()
                .join("Containerfile")
                .to_string_lossy()
                .into(),
            context.path().to_string_lossy().into(),
        ],
        crate::process::RunOptions {
            env: Some(env.clone()),
            cancel: cancel.clone(),
            timeout: Some(Duration::from_secs(seconds)),
            max_output: 1024 * 1024,
            ..Default::default()
        },
    )
    .await
    .context("Could not prepare Crow's managed toolchain image")?;
    image_id(&inspect().await?.stdout)
}

fn image_id(output: &str) -> Result<String> {
    let id = output
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(output.trim());
    anyhow::ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Runtime returned an invalid image ID"
    );
    Ok(format!("sha256:{id}"))
}
