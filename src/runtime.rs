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
    let mut node_packages = BTreeMap::new();
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
                "rust-toolchain",
                "rust-toolchain.toml",
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
        let mut toolchain_files = Vec::new();
        let mut setup_directory = directory.to_owned();
        let (setup, tests, start) = match language {
            "node" => {
                let package: Value = blob
                    .as_ref()
                    .ok()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                node_packages.insert(directory.to_owned(), package.clone());
                let owner = node_workspace_owner(directory, &package, &files, &node_packages);
                let (selected, blocked) = match owner {
                    Ok(Some(owner)) => {
                        setup_directory = owner.clone();
                        (node_packages[&owner].clone(), None)
                    }
                    Ok(None) => (package.clone(), None),
                    Err(note) => (package.clone(), Some(note)),
                };
                package_manager = selected["packageManager"]
                    .as_str()
                    .map(|value| Value::String(value.chars().take(256).collect()))
                    .unwrap_or(Value::Null);
                let (mut setup, mut runner, note) = node_setup(
                    &selected,
                    project_file(&files, &setup_directory, "pnpm-lock.yaml"),
                    project_file(&files, &setup_directory, "yarn.lock"),
                    project_file(&files, &setup_directory, "package-lock.json"),
                );
                warning = blocked.clone().or(note).map_or(Value::Null, Value::String);
                if blocked.is_some() {
                    setup.clear();
                    runner.clear();
                }
                (
                    setup,
                    if !runner.is_empty() && package["scripts"]["test"].is_string() {
                        format!("{runner} test")
                    } else {
                        String::new()
                    },
                    if !runner.is_empty() && package["scripts"]["start"].is_string() {
                        format!("{runner} start")
                    } else if !runner.is_empty() && package["scripts"]["dev"].is_string() {
                        format!("{runner} run dev")
                    } else {
                        String::new()
                    },
                )
            }
            "python" => {
                let (setup, test) = python_setup(directory, has("requirements.txt"));
                (setup, test, String::new())
            }
            "rust" => {
                toolchain_files = rust_toolchains(&files, directory);
                if !toolchain_files.is_empty() {
                    warning = json!(
                        "Inspect the toolchain files and Cargo.toml rust-version. The managed image uses stock Alpine rustc/cargo, which do not enforce rust-toolchain pins. Check rustc --version and cargo --version before testing. If the required exact version is unavailable, report the mismatch; an operator-provided image is needed for that version."
                    );
                }
                (
                    "cargo fetch --locked".into(),
                    "cargo test --offline --locked".into(),
                    String::new(),
                )
            }
            "go" => (
                "go mod download".into(),
                "GOPROXY=off go test ./...".into(),
                String::new(),
            ),
            _ => unreachable!(),
        };
        projects.push(json!({"directory":directory,"setupDirectory":setup_directory,"manifest":path,"language":language,"setup":setup,"test":tests,"start":start,"packageManager":package_manager,"toolchainFiles":toolchain_files,"warning":warning}));
    }
    evidence.truncate(100);
    Ok(
        json!({"revision":revision,"commit":commit,"projects":projects,"projectCount":project_count,"projectsTruncated":project_count > projects.len(),"instructionsToInspect":evidence,"browser":{"module":"/opt/browser/node_modules/playwright-core/index.mjs","executable":"/usr/bin/chromium","args":["--no-sandbox","--disable-dev-shm-usage"]},"note":"These are setup candidates, not verified commands. Run setup in setupDirectory, and test/start in directory. Workspace members share their owner's setup. Read CI and manifests, respect runtime versions, select affected projects, and repair failed setup using logs. Do not change application code to make a test pass. Static sites and standard-library projects need no dependency installation."}),
    )
}

fn project_file(files: &[&str], directory: &str, name: &str) -> bool {
    let path = if directory == "." {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    };
    files.contains(&path.as_str())
}

// Support literal components, * and ** without pretending to implement every
// package manager's glob language. Ambiguous declarations need inspection.
fn workspace_member(patterns: &Value, path: &str) -> Option<bool> {
    let patterns = patterns
        .as_array()
        .or_else(|| patterns.get("packages")?.as_array())?;
    if patterns.len() > 256 || path.split('/').count() > 128 {
        return None;
    }
    let mut matched = false;
    for pattern in patterns {
        let pattern = pattern.as_str()?.trim_end_matches('/');
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let parts: Vec<_> = pattern.split('/').collect();
        if parts.len() > 64
            || parts.iter().any(|part| {
                part.is_empty()
                    || *part == "."
                    || *part == ".."
                    || (*part != "*"
                        && *part != "**"
                        && part.chars().any(|c| "*?![]{}\\()".contains(c)))
            })
        {
            return None;
        }
        let path: Vec<_> = path.split('/').collect();
        // Dynamic programming avoids exponential matching of repeated **.
        let mut previous = vec![false; path.len() + 1];
        previous[0] = true;
        for part in parts {
            let mut next = vec![false; path.len() + 1];
            if part == "**" {
                next[0] = previous[0];
            }
            for index in 1..=path.len() {
                next[index] = if part == "**" {
                    previous[index] || next[index - 1]
                } else {
                    previous[index - 1] && (part == "*" || part == path[index - 1])
                };
            }
            previous = next;
        }
        matched |= previous[path.len()];
    }
    Some(matched)
}

fn node_lockfile(files: &[&str], directory: &str) -> bool {
    [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ]
    .iter()
    .any(|name| project_file(files, directory, name))
}

fn nested_workspace_ambiguity(
    directory: &str,
    files: &[&str],
    packages: &BTreeMap<String, Value>,
) -> Option<String> {
    let mut ancestor = directory;
    while ancestor != "." {
        ancestor = ancestor.rsplit_once('/').map_or(".", |(parent, _)| parent);
        if packages
            .get(ancestor)
            .is_some_and(|package| package.get("workspaces").is_some())
            || project_file(files, ancestor, "pnpm-workspace.yaml")
        {
            return Some(format!(
                "Inspect package-manager pins and lockfiles in {directory} and workspace configuration in {ancestor} before choosing setup or test commands. Nested pins or lockfiles do not establish an independent workspace; command candidates are withheld."
            ));
        }
    }
    None
}

fn node_workspace_owner(
    directory: &str,
    package: &Value,
    files: &[&str],
    packages: &BTreeMap<String, Value>,
) -> std::result::Result<Option<String>, String> {
    if project_file(files, directory, "pnpm-workspace.yaml") {
        return Err(format!(
            "Inspect {directory}/pnpm-workspace.yaml and its root package.json before choosing setup or test commands. Workspace membership has not been resolved, so no npm fallback is suggested."
        ));
    }
    if directory == "." {
        return Ok(None);
    }
    if package.get("packageManager").is_some() || node_lockfile(files, directory) {
        return nested_workspace_ambiguity(directory, files, packages).map_or(Ok(None), Err);
    }
    let mut ancestor = directory;
    let mut fallback = None;
    loop {
        ancestor = ancestor.rsplit_once('/').map_or(".", |(parent, _)| parent);
        let pinned = packages
            .get(ancestor)
            .is_some_and(|package| package.get("packageManager").is_some());
        let locked = node_lockfile(files, ancestor);
        if (pinned || locked)
            && let Some(warning) = nested_workspace_ambiguity(ancestor, files, packages)
        {
            return Err(warning);
        }
        let independent = pinned || locked;
        if project_file(files, ancestor, "pnpm-workspace.yaml") {
            return Err(format!(
                "Inspect {ancestor}/pnpm-workspace.yaml and its root package.json before choosing setup or test commands. Workspace membership has not been resolved, so no npm fallback is suggested."
            ));
        }
        if let Some(parent) = packages.get(ancestor)
            && let Some(patterns) = parent.get("workspaces")
        {
            let relative = if ancestor == "." {
                directory
            } else {
                &directory[ancestor.len() + 1..]
            };
            match workspace_member(patterns, relative) {
                Some(true) => {
                    if let Some(root) = node_workspace_owner(ancestor, parent, files, packages)? {
                        // Yarn 2+ recursively discovers workspace declarations in
                        // member packages. Do not assume npm or unknown Yarn
                        // versions install the same nested workspace graph.
                        let modern_yarn = packages[&root]["packageManager"]
                            .as_str()
                            .and_then(pinned_manager)
                            .is_some_and(|(manager, version)| {
                                manager == "yarn"
                                    && version
                                        .split('.')
                                        .next()
                                        .and_then(|major| major.parse::<u64>().ok())
                                        .is_some_and(|major| major >= 2)
                            });
                        if !modern_yarn {
                            return Err(format!(
                                "Inspect nested workspaces in {ancestor}/package.json and {root}/package.json before choosing setup or test commands. Automatic nested-workspace inheritance requires an exact Yarn 2+ pin; no npm fallback is suggested."
                            ));
                        }
                        return Ok(Some(root));
                    }
                    if independent || ancestor == "." {
                        return Ok(Some(ancestor.to_owned()));
                    }
                    // An unpinned intermediate declaration may not itself be
                    // a workspace member even when an outer root directly
                    // includes this project. Prefer that root if one exists.
                    fallback.get_or_insert_with(|| ancestor.to_owned());
                }
                // This declaration does not own the project, but a more
                // distant root may include it through a recursive pattern.
                // Check independent-package boundaries before continuing.
                Some(false) => {}
                None => {
                    return Err(format!(
                        "Inspect workspace patterns in {ancestor}/package.json before choosing this project's setup or test commands. Workspace membership is ambiguous, so no npm fallback is suggested."
                    ));
                }
            }
        }
        if independent || ancestor == "." {
            // Do not walk through an independent nested package and inherit
            // a more distant workspace root's recursive patterns.
            return Ok(fallback);
        }
    }
}

fn python_setup(directory: &str, requirements: bool) -> (String, String) {
    // Independent Python projects may require incompatible package versions.
    // Hash the directory so repository filenames never become shell syntax.
    let prefix = format!(
        "/workspace/.crow-tools/python/{}",
        fingerprint(&[directory])
    );
    let install = if requirements {
        "-r requirements.txt"
    } else {
        "-e ."
    };
    let environment = format!("VIRTUAL_ENV={prefix} PATH=\"{prefix}/bin:$PATH\"");
    (
        format!(
            "python3 -m venv {prefix} && {environment} {prefix}/bin/python -m pip install {install}"
        ),
        format!("{environment} {prefix}/bin/python -m pytest"),
    )
}

fn rust_toolchains(files: &[&str], directory: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut ancestors: Vec<_> = directory.split('/').filter(|part| *part != ".").collect();
    loop {
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            let path = if ancestors.is_empty() {
                name.to_owned()
            } else {
                format!("{}/{name}", ancestors.join("/"))
            };
            if files.contains(&path.as_str()) {
                result.push(path);
            }
        }
        if ancestors.pop().is_none() {
            break;
        }
    }
    result
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
    // A prepared monorepo can contain projects with different manager pins.
    // Separate installations keep later setup from replacing an earlier CLI.
    let identity = fingerprint(&[manager, version.unwrap_or("latest")]);
    let prefix = format!("/workspace/.crow-tools/{manager}/{identity}");
    let binaries = format!("{prefix}/node_modules/.bin");
    let binary = format!("{binaries}/{manager}");
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
                format!("PATH=\"{binaries}:$PATH\" {binary}"),
                format!("npm install --prefix {prefix} --no-audit --no-fund {package} && "),
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
    fn own_pnpm_workspace_requires_inspection_before_root_pin_or_lock_shortcuts() {
        for directory in [".", "tools/project"] {
            let path = if directory == "." {
                "pnpm-workspace.yaml".to_owned()
            } else {
                format!("{directory}/pnpm-workspace.yaml")
            };
            let lock = if directory == "." {
                "pnpm-lock.yaml".to_owned()
            } else {
                format!("{directory}/pnpm-lock.yaml")
            };
            for package in [json!({}), json!({"packageManager":"pnpm@9.15.4"})] {
                for files in [vec![path.as_str()], vec![path.as_str(), lock.as_str()]] {
                    let warning =
                        node_workspace_owner(directory, &package, &files, &BTreeMap::new())
                            .unwrap_err();
                    assert!(warning.contains("pnpm-workspace.yaml"), "{warning}");
                }
            }
        }
    }

    #[test]
    fn workspace_members_inherit_only_their_owner_and_respect_boundaries() {
        let packages = BTreeMap::from([
            (
                ".".to_owned(),
                json!({"packageManager":"yarn@4.9.2","workspaces":["packages/*"]}),
            ),
            (
                "nested".to_owned(),
                json!({"packageManager":"npm@10.9.0","workspaces":{"packages":["apps/**"]}}),
            ),
        ]);
        let files = [
            "package.json",
            "yarn.lock",
            "nested/package.json",
            "nested/package-lock.json",
        ];
        assert_eq!(
            node_workspace_owner("packages/app", &json!({}), &files, &packages),
            Ok(Some(".".into()))
        );
        assert!(node_workspace_owner("nested/apps/api", &json!({}), &files, &packages).is_err());
        assert_eq!(
            node_workspace_owner("examples/independent", &json!({}), &files, &packages),
            Ok(None)
        );
        assert!(
            node_workspace_owner(
                "packages/app",
                &json!({"packageManager":"npm@10.9.1"}),
                &files,
                &packages
            )
            .unwrap_err()
            .contains("Nested pins or lockfiles")
        );
        for lock in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ] {
            let path = format!("packages/app/{lock}");
            let mut files = files.to_vec();
            files.push(&path);
            assert!(node_workspace_owner("packages/app", &json!({}), &files, &packages).is_err());
        }
        assert!(
            node_workspace_owner(
                "packages/app",
                &json!({}),
                &["pnpm-workspace.yaml"],
                &packages
            )
            .unwrap_err()
            .contains("pnpm-workspace.yaml")
        );
        assert_eq!(
            workspace_member(&json!(["packages/*"]), "packages/app/deep"),
            Some(false)
        );
        assert_eq!(
            workspace_member(&json!(["packages/**"]), "packages/app/deep"),
            Some(true)
        );
        assert_eq!(
            workspace_member(&json!(["packages/*", "!packages/excluded"]), "packages/app"),
            None
        );
        assert_eq!(
            workspace_member(&json!(["packages/{app,api}"]), "packages/app"),
            None
        );
        let mut nested = packages.clone();
        nested.get_mut(".").unwrap()["workspaces"] = json!(["packages/**"]);
        nested.insert(
            "packages/tool".into(),
            json!({"packageManager":"npm@10.9.0"}),
        );
        let nested_files = [
            "package.json",
            "yarn.lock",
            "packages/tool/package-lock.json",
        ];
        assert!(
            node_workspace_owner(
                "packages/tool/examples/demo",
                &json!({}),
                &nested_files,
                &nested
            )
            .is_err()
        );
        nested.get_mut("packages/tool").unwrap()["workspaces"] = json!(["examples/*"]);
        assert!(
            node_workspace_owner(
                "packages/tool/examples/demo",
                &json!({}),
                &nested_files,
                &nested
            )
            .is_err()
        );
    }

    #[test]
    fn nested_yarn_workspaces_resolve_ultimate_root_without_crossing_independent_pins() {
        let mut packages = BTreeMap::from([
            (
                ".".to_owned(),
                json!({"packageManager":"yarn@4.9.2","workspaces":["packages/*"]}),
            ),
            (
                "packages/app".to_owned(),
                json!({"workspaces":["plugins/*"]}),
            ),
        ]);
        let files = ["package.json", "yarn.lock", "packages/app/package.json"];
        assert_eq!(
            node_workspace_owner("packages/app/plugins/foo", &json!({}), &files, &packages),
            Ok(Some(".".into()))
        );
        assert_eq!(
            node_workspace_owner("packages/app", &packages["packages/app"], &files, &packages),
            Ok(Some(".".into()))
        );
        for manager in ["npm@10.9.0", "yarn@1.22.22"] {
            packages.get_mut(".").unwrap()["packageManager"] = json!(manager);
            assert!(
                node_workspace_owner("packages/app/plugins/foo", &json!({}), &files, &packages)
                    .unwrap_err()
                    .contains("nested workspaces")
            );
        }
        packages.get_mut(".").unwrap()["packageManager"] = json!("yarn@4.9.2");
        packages.get_mut("packages/app").unwrap()["packageManager"] = json!("npm@10.9.0");
        assert!(
            node_workspace_owner("packages/app/plugins/foo", &json!({}), &files, &packages)
                .unwrap_err()
                .contains("Nested pins or lockfiles")
        );
        packages
            .get_mut("packages/app")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("packageManager");
        let mut files = files.to_vec();
        files.push("packages/app/package-lock.json");
        assert!(
            node_workspace_owner("packages/app/plugins/foo", &json!({}), &files, &packages)
                .is_err()
        );
    }

    #[test]
    fn outer_workspace_members_survive_nonmatching_intermediate_declarations() {
        let mut packages = BTreeMap::from([
            (
                ".".to_owned(),
                json!({"packageManager":"yarn@4.9.2","workspaces":["packages/**"]}),
            ),
            (
                "packages/app".to_owned(),
                json!({"workspaces":["plugins/*"]}),
            ),
        ]);
        let files = ["package.json", "yarn.lock", "packages/app/package.json"];
        let helper = "packages/app/tools/helper";
        assert_eq!(
            node_workspace_owner(helper, &json!({}), &files, &packages),
            Ok(Some(".".into()))
        );
        // Both pins and lockfiles require inspection under an outer workspace.
        packages.get_mut("packages/app").unwrap()["packageManager"] = json!("npm@10.9.0");
        assert!(
            node_workspace_owner(helper, &json!({}), &files, &packages)
                .unwrap_err()
                .contains("Nested pins or lockfiles")
        );
        packages
            .get_mut("packages/app")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("packageManager");
        for lock in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ] {
            let path = format!("packages/app/{lock}");
            let mut files = files.to_vec();
            files.push(&path);
            assert!(
                node_workspace_owner(helper, &json!({}), &files, &packages).is_err(),
                "{lock} requires inspection"
            );
        }
    }

    #[test]
    fn directly_selected_deep_members_prefer_root_over_unpinned_intermediate_match() {
        let plugin = "packages/app/plugins/foo";
        for (root_pattern, root_manager, expected) in [
            ("packages/*/plugins/*", "yarn@4.9.2", Some(".")),
            ("packages/*/plugins/*", "npm@10.9.0", Some(".")),
            ("elsewhere/*", "yarn@4.9.2", Some("packages/app")),
            ("packages/*", "yarn@4.9.2", Some(".")),
            ("packages/*", "npm@10.9.0", None),
        ] {
            let mut packages = BTreeMap::from([
                (
                    ".".to_owned(),
                    json!({"packageManager":root_manager,"workspaces":[root_pattern]}),
                ),
                (
                    "packages/app".to_owned(),
                    json!({"workspaces":["plugins/*"]}),
                ),
            ]);
            let files = ["package.json", "packages/app/package.json"];
            let owner = node_workspace_owner(plugin, &json!({}), &files, &packages);
            if let Some(expected) = expected {
                assert_eq!(
                    owner,
                    Ok(Some(expected.into())),
                    "{root_pattern} {root_manager}"
                );
            } else {
                assert!(owner.unwrap_err().contains("nested workspaces"));
            }
            packages.get_mut("packages/app").unwrap()["packageManager"] = json!("npm@10.9.0");
            assert!(
                node_workspace_owner(plugin, &json!({}), &files, &packages)
                    .unwrap_err()
                    .contains("Nested pins or lockfiles")
            );
            packages
                .get_mut("packages/app")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("packageManager");
            for lock in [
                "package-lock.json",
                "npm-shrinkwrap.json",
                "pnpm-lock.yaml",
                "yarn.lock",
            ] {
                let path = format!("packages/app/{lock}");
                let mut files = files.to_vec();
                files.push(&path);
                assert!(
                    node_workspace_owner(plugin, &json!({}), &files, &packages).is_err(),
                    "{lock}"
                );
            }
        }
    }

    #[test]
    fn nested_pins_and_lockfiles_require_inspection_under_workspaces() {
        let pinned = json!({"packageManager":"yarn@4.9.2"});
        let mut packages = BTreeMap::from([
            (
                ".".into(),
                json!({"packageManager":"yarn@4.9.2","workspaces":["packages/**"]}),
            ),
            ("packages/app".into(), pinned.clone()),
        ]);
        let files = ["package.json", "yarn.lock"];
        for (directory, package) in [
            ("packages/app", pinned.clone()),
            ("packages/app/tools/helper", json!({})),
            ("packages/direct", pinned.clone()),
        ] {
            assert!(
                node_workspace_owner(directory, &package, &files, &packages)
                    .unwrap_err()
                    .contains("Nested pins or lockfiles")
            );
        }
        assert_eq!(
            node_workspace_owner(".", &pinned, &files, &packages),
            Ok(None)
        );
        packages
            .get_mut(".")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("workspaces");
        assert_eq!(
            node_workspace_owner("packages/app", &pinned, &files, &packages),
            Ok(None)
        );
        assert_eq!(
            node_workspace_owner("packages/app/tools/helper", &json!({}), &files, &packages),
            Ok(None)
        );
        assert!(
            node_workspace_owner("packages/app", &pinned, &["pnpm-workspace.yaml"], &packages)
                .unwrap_err()
                .contains("Nested pins or lockfiles")
        );
        // Intervening locks cannot establish independence from a farther root.
        packages.get_mut(".").unwrap()["workspaces"] = json!(["packages/**"]);
        assert!(
            node_workspace_owner(
                "packages/app/tools/helper",
                &pinned,
                &["packages/app/yarn.lock"],
                &packages
            )
            .is_err()
        );
        packages
            .get_mut(".")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("workspaces");
        packages.get_mut("packages/app").unwrap()["workspaces"] = json!(["tools/*"]);
        assert_eq!(
            node_workspace_owner(
                "packages/app/tools/helper",
                &json!({}),
                &["packages/app/yarn.lock"],
                &packages
            ),
            Ok(Some("packages/app".into()))
        );
    }

    #[test]
    fn python_projects_keep_separate_environments_and_safe_nested_paths() {
        let first = python_setup("services/first", true);
        let second = python_setup("services/second", false);
        assert_ne!(first.1, second.1);
        assert!(first.0.ends_with("-m pip install -r requirements.txt"));
        assert!(second.0.ends_with("-m pip install -e ."));
        for directory in [
            ".",
            "services/first",
            "services/second",
            "$(touch /tmp/injected); project",
        ] {
            let (setup, test) = python_setup(directory, true);
            let prefix = format!(
                "/workspace/.crow-tools/python/{}",
                fingerprint(&[directory])
            );
            assert!(setup.starts_with(&format!("python3 -m venv {prefix} && ")));
            assert_eq!(
                test,
                format!(
                    "VIRTUAL_ENV={prefix} PATH=\"{prefix}/bin:$PATH\" {prefix}/bin/python -m pytest"
                )
            );
            assert!(!setup.contains("$("));
            assert!(!test.contains("$("));
        }
        assert_eq!(
            python_setup("services/first", true).1,
            python_setup("services/first", false).1
        );
    }

    #[test]
    fn rust_toolchain_discovery_checks_project_and_ancestor_pins() {
        let files = [
            "rust-toolchain.toml",
            "crates/rust-toolchain",
            "crates/api/rust-toolchain.toml",
            "other/rust-toolchain.toml",
        ];
        assert_eq!(
            rust_toolchains(&files, "crates/api"),
            [
                "crates/api/rust-toolchain.toml",
                "crates/rust-toolchain",
                "rust-toolchain.toml"
            ]
        );
        assert_eq!(rust_toolchains(&files, "."), ["rust-toolchain.toml"]);
        assert!(rust_toolchains(&["other/rust-toolchain"], "crates/api").is_empty());
    }

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
            let (manager, version) = pinned_manager(declaration).unwrap();
            let prefix = format!(
                "/workspace/.crow-tools/{manager}/{}",
                fingerprint(&[manager, version])
            );
            assert!(runner.starts_with(&format!("PATH=\"{prefix}/node_modules/.bin:$PATH\" ")));
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

    #[test]
    fn projects_with_different_manager_pins_keep_separate_installations() {
        for (manager, first, second) in [
            ("npm", "10.9.0", "10.9.1"),
            ("pnpm", "9.15.4", "10.0.0"),
            ("yarn", "1.22.22", "4.9.2"),
        ] {
            let first = node_setup(
                &json!({"packageManager":format!("{manager}@{first}")}),
                false,
                false,
                true,
            );
            let second = node_setup(
                &json!({"packageManager":format!("{manager}@{second}")}),
                false,
                false,
                true,
            );
            let prefix = |setup: &str| {
                setup
                    .split("--prefix ")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_owned()
            };
            let first_prefix = prefix(&first.0);
            let second_prefix = prefix(&second.0);
            assert_ne!(first_prefix, second_prefix);
            for (setup, runner, _) in [&first, &second] {
                let prefix = prefix(setup);
                assert_eq!(
                    *runner,
                    format!(
                        "PATH=\"{prefix}/node_modules/.bin:$PATH\" {prefix}/node_modules/.bin/{manager}"
                    )
                );
                assert!(setup.contains(runner));
            }
        }
        let plain = node_setup(&json!({}), false, false, false);
        assert_eq!(plain.1, "npm");
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
        let prefix = format!(
            "/workspace/.crow-tools/yarn/{}",
            fingerprint(&["yarn", "4.9.2"])
        );
        assert_eq!(
            found["projects"][0]["test"],
            format!(
                "PATH=\"{prefix}/node_modules/.bin:$PATH\" {prefix}/node_modules/.bin/yarn test"
            )
        );
        assert_eq!(
            found["projects"][0]["start"],
            format!(
                "PATH=\"{prefix}/node_modules/.bin:$PATH\" {prefix}/node_modules/.bin/yarn run dev"
            )
        );
    }
}
