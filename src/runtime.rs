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
    let mut project_count = 0;
    // Root manifests and shallow packages should survive discovery limits even
    // when fixture directories contain hundreds of manifests.
    let mut candidates = files.clone();
    candidates.sort_by_key(|path| (path.bytes().filter(|b| *b == b'/').count(), *path));
    for path in &candidates {
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
        project_count += 1;
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
        let mut package_manager = Value::Null;
        let mut warning = Value::Null;
        let (setup, tests, start) = match language {
            "node" => {
                let package: Value = blob
                    .as_ref()
                    .ok()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                package_manager = package["packageManager"]
                    .as_str()
                    .map(|value| Value::String(value.chars().take(256).collect()))
                    .unwrap_or(Value::Null);
                let (setup, runner, note) = node_setup(
                    &package,
                    has("pnpm-lock.yaml"),
                    has("yarn.lock"),
                    has("package-lock.json"),
                );
                warning = note.map_or(Value::Null, Value::String);
                (
                    setup,
                    if package["scripts"]["test"].is_string() {
                        format!("{runner} test")
                    } else {
                        String::new()
                    },
                    if package["scripts"]["start"].is_string() {
                        format!("{runner} start")
                    } else if package["scripts"]["dev"].is_string() {
                        format!("{runner} run dev")
                    } else {
                        String::new()
                    },
                )
            }
            "python" => (
                if has("requirements.txt") {
                    "python3 -m venv /workspace/.venv && /workspace/.venv/bin/pip install -r requirements.txt"
                } else {
                    "python3 -m venv /workspace/.venv && /workspace/.venv/bin/pip install -e ."
                }.to_owned(),
                "/workspace/.venv/bin/python -m pytest".to_owned(),
                String::new(),
            ),
            "rust" => ("cargo fetch --locked".into(), "cargo test --offline --locked".into(), String::new()),
            "go" => ("go mod download".into(), "GOPROXY=off go test ./...".into(), String::new()),
            _ => unreachable!(),
        };
        projects.push(json!({"directory":directory,"manifest":path,"language":language,"setup":setup,"test":tests,"start":start,"packageManager":package_manager,"warning":warning}));
    }
    evidence.truncate(100);
    Ok(
        json!({"revision":revision,"commit":commit,"projects":projects,"projectCount":project_count,"projectsTruncated":project_count > projects.len(),"instructionsToInspect":evidence,"browser":{"module":"/opt/browser/node_modules/playwright-core/index.mjs","executable":"/usr/bin/chromium","args":["--no-sandbox","--disable-dev-shm-usage"]},"note":"These are setup candidates, not verified commands. Read CI and manifests, respect runtime versions, select affected projects, and repair failed setup using logs. Do not change application code to make a test pass. Static sites and standard-library projects need no dependency installation."}),
    )
}

/// Accept exact registry versions only. Never interpolate repository strings as
/// shell syntax, npm aliases, URLs, ranges, or executable package specifications.
fn pinned_manager(value: &str) -> Option<(&str, &str)> {
    let (manager, requested) = value.split_once('@')?;
    if !["npm", "pnpm", "yarn"].contains(&manager)
        || requested.len() > 256
        || !requested
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return None;
    }
    let version = requested
        .split_once('+')
        .map_or(requested, |(version, _)| version);
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let numbers: Vec<_> = core.split('.').collect();
    if numbers.len() != 3
        || numbers
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        || version.ends_with('-')
    {
        return None;
    }
    Some((manager, version))
}

fn node_setup(
    package: &Value,
    pnpm_lock: bool,
    yarn_lock: bool,
    npm_lock: bool,
) -> (String, String, Option<String>) {
    let declared = package["packageManager"].as_str();
    let pinned = declared.and_then(pinned_manager);
    let manager = pinned.map(|(manager, _)| manager).unwrap_or(if pnpm_lock {
        "pnpm"
    } else if yarn_lock {
        "yarn"
    } else {
        "npm"
    });
    let warning = if !package["packageManager"].is_null() && pinned.is_none() {
        Some("packageManager is not a supported exact npm, pnpm or Yarn version. Inspect the manifest and CI before choosing a package manager.".to_owned())
    } else if pinned.is_none() && manager != "npm" {
        Some("No exact packageManager version is declared. Inspect CI and package-manager configuration before using this candidate.".to_owned())
    } else {
        None
    };
    let version = pinned.map(|(_, version)| version);
    let binary = format!("/workspace/.crow-tools/node_modules/.bin/{manager}");
    let (runner, install) = match (manager, version) {
        ("npm", None) => ("npm".to_owned(), String::new()),
        (manager, version) => {
            // Yarn 2+ ships its CLI through @yarnpkg/cli-dist, not the Yarn 1 package.
            let modern_yarn = manager == "yarn"
                && version.is_some_and(|v| {
                    v.split('.')
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .is_some_and(|major| major >= 2)
                });
            let package = if modern_yarn {
                "@yarnpkg/cli-dist"
            } else {
                manager
            };
            let package = version.map_or(package.to_owned(), |v| format!("{package}@{v}"));
            (
                // Lifecycle hooks and nested package scripts invoke the manager by
                // name. Keep those subprocesses on the selected installation too.
                format!("PATH=\"/workspace/.crow-tools/node_modules/.bin:$PATH\" {binary}"),
                format!(
                    "npm install --prefix /workspace/.crow-tools --no-audit --no-fund {package} && "
                ),
            )
        }
    };
    let arguments = match manager {
        "pnpm" => "install --frozen-lockfile",
        "yarn" if version.is_some_and(|v| !v.starts_with("1.") && !v.starts_with("0.")) => {
            "install --immutable"
        }
        "yarn" => "install --frozen-lockfile",
        _ if npm_lock => "ci --no-audit --no-fund",
        _ => "install --no-audit --no-fund",
    };
    (format!("{install}{runner} {arguments}"), runner, warning)
}

pub(crate) fn image_tag(cache: &Path) -> Result<String> {
    // Separate roots may share a Podman store. Give each root its own tag so
    // its cleanup cannot invalidate another worker's managed-image references.
    let owner = std::fs::canonicalize(cache)?;
    let version = fingerprint(&[
        include_str!("runtime/Containerfile"),
        include_str!("runtime/proxy.py"),
        &owner.to_string_lossy(),
    ]);
    Ok(format!("localhost/crow-runtime:{}", &version[..20]))
}
fn record_image(cache: &Path, tag: &str, image: &str) -> Result<()> {
    let registry = cache.join("images");
    crate::util::private_dir(&registry)?;
    crate::util::atomic(
        &registry.join(format!("{}.json", fingerprint(&[tag]))),
        &json!({"tag":tag,"image":image,"lastUsedAt":crate::util::now()}),
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
    let tag = image_tag(cache)?;
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
        let id = image_id(&output.stdout)?;
        record_image(cache, &tag, &id)?;
        return Ok(id);
    }
    let context = tempfile::Builder::new()
        .prefix(".crow-build-")
        .tempdir_in(cache)?;
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
    let id = image_id(&inspect().await?.stdout)?;
    record_image(cache, &tag, &id)?;
    Ok(id)
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

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn package_manager_candidates_respect_exact_versions_and_yarn_generations() {
        for (declaration, package, flag) in [
            ("pnpm@9.15.4", "pnpm@9.15.4", "--frozen-lockfile"),
            ("yarn@1.22.22", "yarn@1.22.22", "--frozen-lockfile"),
            (
                "yarn@4.9.2+sha224.abc123",
                "@yarnpkg/cli-dist@4.9.2",
                "--immutable",
            ),
            ("npm@10.9.0", "npm@10.9.0", "ci --no-audit"),
            ("pnpm@10.0.0-rc.1", "pnpm@10.0.0-rc.1", "--frozen-lockfile"),
        ] {
            let (setup, runner, warning) =
                node_setup(&json!({"packageManager":declaration}), false, false, true);
            assert!(setup.contains(package), "{setup}");
            assert!(setup.contains(flag), "{setup}");
            assert!(runner.starts_with("PATH=\"/workspace/.crow-tools/node_modules/.bin:$PATH\" "));
            assert!(setup.contains(&runner));
            assert!(warning.is_none());
            assert!(!setup.contains("sha224"));
        }
        // Even safe-looking versions must not be interpreted as URLs, aliases or ranges.
        for declaration in [
            "pnpm@latest",
            "yarn@^4.0.0",
            "pnpm@9.0.0;touch /tmp/injected",
            "pnpm@9.0.0$(id)",
            "pnpm@9.0.0`id`",
            "yarn@https://evil.invalid/payload",
            "pnpm@npm:other@9.0.0",
            "pnpm@9.0.0\nfalse",
            "pnpm@9.0.0-",
            "other@9.0.0",
        ] {
            assert!(pinned_manager(declaration).is_none(), "{declaration}");
            let (setup, _, warning) =
                node_setup(&json!({"packageManager":declaration}), false, false, true);
            assert_eq!(setup, "npm ci --no-audit --no-fund");
            assert!(warning.is_some());
        }
    }

    #[tokio::test]
    async fn discovery_keeps_root_projects_and_reports_omitted_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
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
        };
        git(&["init", "-b", "main"]);
        std::fs::write(dir.join("package.json"), r#"{"packageManager":"yarn@4.9.2","scripts":{"test":"node test.js","dev":"node server.js"}}"#).unwrap();
        for index in 0..105 {
            let fixture = dir.join(format!("fixtures/case-{index:03}"));
            std::fs::create_dir_all(&fixture).unwrap();
            std::fs::write(fixture.join("package.json"), "{}").unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-m", "discovery fixture"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let found = discover(&json!({"dir":dir,"head":commit}), "head")
            .await
            .unwrap();
        assert_eq!(found["projects"].as_array().unwrap().len(), 100);
        assert_eq!(found["projectCount"], 106);
        assert_eq!(found["projectsTruncated"], true);
        assert_eq!(found["projects"][0]["directory"], ".");
        assert_eq!(found["projects"][0]["packageManager"], "yarn@4.9.2");
        assert!(
            found["projects"][0]["setup"]
                .as_str()
                .unwrap()
                .contains("@yarnpkg/cli-dist@4.9.2")
        );
        assert_eq!(
            found["projects"][0]["test"],
            "PATH=\"/workspace/.crow-tools/node_modules/.bin:$PATH\" /workspace/.crow-tools/node_modules/.bin/yarn test"
        );
        assert_eq!(
            found["projects"][0]["start"],
            "PATH=\"/workspace/.crow-tools/node_modules/.bin:$PATH\" /workspace/.crow-tools/node_modules/.bin/yarn run dev"
        );
    }
}
