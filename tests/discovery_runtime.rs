//! Verify discovered package-manager commands in the actual isolated runtime.
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
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
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
    exec.call(name, &args, CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn python_discovery_combines_manifests_and_withholds_ambiguous_package_installs() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    for (name, metadata, setup_py, requirements) in [
        (
            "package",
            "[project]\nname='fixture'\nversion='1.0.0'\n",
            false,
            true,
        ),
        ("legacy", "[tool.ruff]\nline-length=88\n", true, true),
        (
            "tool_only",
            "[tool.pytest.ini_options]\naddopts='-q'\n",
            false,
            true,
        ),
        ("config_only", "[tool.ruff]\nline-length=88\n", false, false),
        (
            "ambiguous",
            "[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
            false,
            true,
        ),
        ("malformed", "[project\n", false, true),
        (
            "no_package",
            "[project]\nname='fixture'\nversion='1.0.0'\n[tool.poetry]\npackage-mode=false\n",
            false,
            true,
        ),
    ] {
        let directory = repo.join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("pyproject.toml"), metadata).unwrap();
        if setup_py {
            std::fs::write(
                directory.join("setup.py"),
                "from setuptools import setup\nsetup(name='fixture', version='1.0.0')\n",
            )
            .unwrap();
        }
        if requirements {
            std::fs::write(directory.join("requirements.txt"), "pytest==8.4.2\n").unwrap();
        }
    }
    let oversized = repo.join("oversized");
    std::fs::create_dir(&oversized).unwrap();
    // read_blob rejects files above 2 MiB. One failed manifest read must not
    // prevent discovery of the other projects or hide this requirements file.
    std::fs::write(
        oversized.join("pyproject.toml"),
        format!("#{}", "x".repeat(2 * 1024 * 1024)),
    )
    .unwrap();
    std::fs::write(oversized.join("requirements.txt"), "pytest==8.4.2\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "Python manifest discovery fixtures"],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":temp.path(),"job":{"repo":"fixture/python-discovery","settings":{"execution":{"automatic":true}}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(found["projectCount"], 8, "{found}");
    assert_eq!(projects.len(), 8, "{found}");
    assert_eq!(found["projectsTruncated"], false);
    for project in projects {
        let name = project["directory"].as_str().unwrap();
        let setup = project["setup"].as_str().unwrap();
        assert_eq!(project["manifest"], format!("{name}/pyproject.toml"));
        let manifests = project["manifests"].as_array().unwrap();
        assert_eq!(
            manifests.len(),
            if name == "legacy" {
                3
            } else if name == "config_only" {
                1
            } else {
                2
            }
        );
        if ["package", "legacy"].contains(&name) {
            assert!(
                setup.ends_with("-m pip install -r requirements.txt -e ."),
                "{project}"
            );
            assert!(project["warning"].is_null(), "{project}");
        } else if name == "config_only" {
            assert!(setup.is_empty());
            assert_eq!(project["test"], "");
            assert!(project["warning"].is_string());
        } else {
            assert!(
                setup.ends_with("-m pip install -r requirements.txt"),
                "{project}"
            );
            assert_eq!(
                project["warning"].is_null(),
                name == "tool_only",
                "{project}"
            );
            if name == "oversized" {
                assert!(
                    project["warning"]
                        .as_str()
                        .unwrap()
                        .contains("Could not read pyproject.toml")
                );
            }
        }
    }
}

#[tokio::test]
async fn python_discovery_counts_projects_after_output_limit_not_manifest_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    for index in 0..103 {
        let directory = repo.join(format!("project-{index:03}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("pyproject.toml"),
            "[project]\nname='fixture'\nversion='1.0.0'\n",
        )
        .unwrap();
        std::fs::write(directory.join("requirements.txt"), "pytest\n").unwrap();
        std::fs::write(
            directory.join("setup.py"),
            "from setuptools import setup\nsetup()\n",
        )
        .unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "bounded Python discovery fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":temp.path(),"job":{"repo":"fixture/python-discovery","settings":{"execution":{"automatic":true}}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    assert_eq!(found["projectCount"], 103, "{found}");
    assert_eq!(found["projectsTruncated"], true);
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 100);
    assert!(
        projects
            .iter()
            .all(|project| project["manifests"].as_array().unwrap().len() == 3)
    );
    let directories: std::collections::BTreeSet<_> = projects
        .iter()
        .map(|project| project["directory"].as_str().unwrap())
        .collect();
    assert_eq!(directories.len(), 100);
}

#[tokio::test]
async fn pnpm_workspace_roots_withhold_guessed_commands() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    for directory in [".", "tools/project"] {
        let path = repo.join(directory);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("package.json"),
            r#"{"private":true,"scripts":{"test":"node test.js","dev":"node server.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            path.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
    }
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "commit",
            "-m",
            "pnpm workspace roots without other manager hints",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":temp.path(),"job":{"repo":"fixture/pnpm-workspace","settings":{"execution":{"automatic":true}}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 2, "{found}");
    for project in projects {
        assert!(
            project["warning"]
                .as_str()
                .unwrap()
                .contains("pnpm-workspace.yaml"),
            "{project}"
        );
        for command in ["setup", "test", "start"] {
            assert_eq!(project[command], "", "{project}");
        }
    }
}

#[tokio::test]
async fn ambiguous_pnpm_workspace_exposes_only_exact_cli_bootstrap() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(
        repo.join("package.json"),
        r#"{"private":true,"packageManager":"pnpm@11.10.0","scripts":{"test":"vp test run"}}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
    for (name, package) in [
        ("app", json!({"scripts":{"test":"node test.js"}})),
        (
            "unsafe",
            json!({"packageManager":"pnpm@11.10.0; false","scripts":{"test":"node test.js"}}),
        ),
    ] {
        let directory = repo.join("packages").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("package.json"), package.to_string()).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "workspace with exact CLI pin"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let context = json!({"root":temp.path(),"job":{"repo":"fixture/pnpm-bootstrap","settings":{"execution":{"automatic":true}}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3, "{found}");
    for project in projects {
        assert!(project["warning"].is_string(), "{project}");
        for command in ["setup", "test", "start"] {
            assert_eq!(project[command], "", "{project}");
        }
        let bootstrap = &project["packageManagerBootstrap"];
        if project["directory"] == "." {
            assert_eq!(bootstrap["manager"], "pnpm");
            assert_eq!(bootstrap["version"], "11.10.0");
            let install = bootstrap["install"].as_str().unwrap();
            assert!(install.starts_with("npm install --prefix /workspace/.crow-tools/pnpm/"));
            assert!(install.ends_with("pnpm@11.10.0"));
            assert!(!install.contains("--global"));
            assert!(!install.contains("--frozen-lockfile"));
            assert!(
                bootstrap["runner"]
                    .as_str()
                    .unwrap()
                    .contains("/node_modules/.bin/pnpm")
            );
        } else {
            // Bootstrap metadata must not imply an owner for unresolved workspace members.
            assert!(bootstrap.is_null(), "{project}");
        }
    }
    assert!(
        found["managedImageDefaults"]["scope"]
            .as_str()
            .unwrap()
            .contains("image auto")
    );
    assert!(
        found["managedImageDefaults"]["installed"]
            .as_array()
            .unwrap()
            .contains(&json!("npm"))
    );
    assert!(
        found["managedImageDefaults"]["notInstalledByDefault"]
            .as_array()
            .unwrap()
            .contains(&json!("corepack"))
    );
    assert_eq!(
        found["managedImageDefaults"]["writableDirectories"],
        json!(["/workspace", "/tmp"])
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_ambiguous_workspace_bootstrap_survives_prepared_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("packages/app")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(
        repo.join("package.json"),
        r#"{"private":true,"packageManager":"pnpm@9.15.4","scripts":{"test":"vp test run"}}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/app/package.json"),
        r#"{"name":"app","private":true}"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "exact CLI pin in unresolved workspace"],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/pnpm-bootstrap","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let project = found["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["directory"] == ".")
        .unwrap();
    assert!(
        project["warning"]
            .as_str()
            .unwrap()
            .contains("pnpm-workspace.yaml")
    );
    for command in ["setup", "test", "start"] {
        assert_eq!(project[command], "", "{project}");
    }
    let bootstrap = &project["packageManagerBootstrap"];
    assert_eq!(bootstrap["version"], "9.15.4");
    let setup_purpose =
        "Install the pinned package manager without guessing workspace dependencies";
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":bootstrap["install"],"purpose":setup_purpose}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    assert_eq!(setup["purpose"], setup_purpose);
    assert_eq!(setup["command"], bootstrap["install"]);
    let test_purpose = "Check the pinned package manager works offline in the prepared environment";
    let command = format!("{} --version", bootstrap["runner"].as_str().unwrap());
    let result = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":setup["id"],"command":command,"purpose":test_purpose}),
    ).await;
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(
        result["stdout"].as_str().unwrap().trim(),
        "9.15.4",
        "{result}"
    );
    assert_eq!(result["purpose"], test_purpose);
    assert_eq!(result["environment"], setup["id"]);
    let saved = call(&exec, "list_experiments", json!({})).await;
    for receipt in [&setup, &result] {
        assert!(
            saved["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["id"] == receipt["id"]
                    && record["purpose"] == receipt["purpose"]),
            "{saved}"
        );
    }
    println!(
        "Ambiguous pnpm workspace retained empty project command candidates; the discovered CLI bootstrap installed pnpm 9.15.4 through the restricted gateway and the saved environment ran it offline with purpose labels intact."
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_pinned_managers_and_nested_commands_run_offline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    for (manager, version, lockfile, lock) in [
        (
            "npm",
            "10.9.0",
            "package-lock.json",
            r#"{"name":"crow-discovery-fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"crow-discovery-fixture","version":"1.0.0","hasInstallScript":true}}}"#,
        ),
        (
            "pnpm",
            "9.15.4",
            "pnpm-lock.yaml",
            "lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n",
        ),
        (
            "yarn",
            "4.9.2",
            "yarn.lock",
            "# This file is generated by running \"yarn install\" inside your project.\n# Manual changes might be lost - proceed with caution!\n\n__metadata:\n  version: 8\n  cacheKey: 10c0\n\n\"crow-discovery-fixture@workspace:.\":\n  version: 0.0.0-use.local\n  resolution: \"crow-discovery-fixture@workspace:.\"\n  languageName: unknown\n  linkType: soft\n",
        ),
    ] {
        let temp = tempfile::tempdir_in(&root).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        let mut scripts = json!({"test":"node test.cjs", "start":"node test.cjs"});
        if manager == "npm" {
            // This checks the installation lifecycle as well as later test/start
            // shells. An absolute npm entrypoint alone leaves nested npm on PATH.
            scripts["postinstall"] = json!("node test.cjs");
        }
        std::fs::write(repo.join("package.json"), json!({"name":"crow-discovery-fixture","version":"1.0.0","private":true,"packageManager":format!("{manager}@{version}"),"scripts":scripts}).to_string()).unwrap();
        std::fs::write(repo.join(lockfile), lock).unwrap();
        std::fs::write(repo.join("test.cjs"), format!("const assert=require('node:assert/strict'); const {{execFileSync}}=require('node:child_process'); assert(process.env.npm_config_user_agent.includes('{manager}/{version}')); assert.equal(execFileSync('{manager}', ['--version'], {{encoding:'utf8'}}).trim(), '{version}'); console.log('verified {manager}/{version} including nested command');\n")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "package manager fixture"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
        let context = json!({"root":root,"job":{"repo":"fixture/discovery","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
        let exec = Execution::from_context(&context, &temp.path().join("experiments"))
            .unwrap()
            .unwrap();
        let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
        let project = &found["projects"][0];
        let setup = call(
            &exec,
            "prepare_environment",
            json!({"revision":"head","setup":project["setup"]}),
        )
        .await;
        assert_eq!(setup["status"], "passed", "{manager} preparation: {setup}");
        if manager == "npm" {
            assert!(
                setup["stdout"].as_str().unwrap().contains(&format!(
                    "verified {manager}/{version} including nested command"
                )),
                "npm lifecycle probe did not run: {setup}"
            );
        }
        let result = call(
            &exec,
            "run_experiment",
            json!({"revision":"head","environment":setup["id"],"command":format!("{} && {}", project["test"].as_str().unwrap(), project["start"].as_str().unwrap())}),
        )
        .await;
        assert_eq!(result["status"], "passed", "{manager} test: {result}");
        assert!(result["stdout"].as_str().unwrap().contains(&format!(
            "verified {manager}/{version} including nested command"
        )));
        println!(
            "Discovered {manager}@{version}: setup passed; pinned manager ran test/start and nested commands offline."
        );
    }
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_pinned_managers_install_without_lockfiles_in_ci() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    for (manager, version, lockfile) in [
        ("pnpm", "9.15.4", "pnpm-lock.yaml"),
        ("yarn", "1.22.22", "yarn.lock"),
        ("yarn", "4.9.2", "yarn.lock"),
    ] {
        let temp = tempfile::tempdir_in(&root).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        std::fs::write(
            repo.join("package.json"),
            json!({
                "name":"crow-no-lock-fixture","version":"1.0.0","private":true,
                "packageManager":format!("{manager}@{version}"),
                "dependencies":{"is-number":"7.0.0"},
                "scripts":{"test":"node test.cjs"}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            repo.join("test.cjs"),
            format!(
                r#"const assert = require('node:assert/strict');
const fs = require('node:fs');
const {{execFileSync}} = require('node:child_process');
assert.equal(process.env.CI, 'true');
assert.equal(process.env.YARN_ENABLE_IMMUTABLE_INSTALLS, undefined);
assert.equal(execFileSync('{manager}', ['--version'], {{encoding:'utf8'}}).trim(), '{version}');
assert.equal(require('is-number')('42'), true);
assert.equal(require('is-number')('not a number'), false);
assert(fs.readFileSync('{lockfile}', 'utf8').includes('is-number'));
console.log('generated {lockfile} and used dependency offline with {manager}@{version}');
"#
            ),
        )
        .unwrap();
        if version == "4.9.2" {
            // Preserve downloaded PnP dependencies inside the prepared snapshot.
            std::fs::write(repo.join(".yarnrc.yml"), "enableGlobalCache: false\n").unwrap();
        }
        git(&repo, &["add", "."]);
        git(
            &repo,
            &["commit", "-m", "pinned manager without a lockfile"],
        );
        assert!(!repo.join(lockfile).exists());
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let config = json!({
            "podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),
            "automatic":true
        });
        let context = json!({
            "root":root,"job":{"repo":"fixture/discovery-no-lock","settings":{"execution":config}},
            "source":{"dir":repo,"head":commit,"base":commit}
        });
        let exec = Execution::from_context(&context, &temp.path().join("experiments"))
            .unwrap()
            .unwrap();
        let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
        let project = &found["projects"][0];
        assert!(project["warning"].is_null(), "{project}");
        let setup = call(
            &exec,
            "prepare_environment",
            json!({"revision":"head","setup":format!(
                "test \"$CI\" = true && test ! -e {lockfile} && {} && test -s {lockfile}",
                project["setup"].as_str().unwrap()
            )}),
        )
        .await;
        assert_eq!(
            setup["status"], "passed",
            "{manager}@{version} setup: {setup}"
        );
        let result = call(
            &exec,
            "run_experiment",
            json!({"revision":"head","environment":setup["id"],"command":project["test"]}),
        )
        .await;
        assert_eq!(
            result["status"], "passed",
            "{manager}@{version} test: {result}"
        );
        assert!(
            result["stdout"].as_str().unwrap().contains(&format!(
                "generated {lockfile} and used dependency offline with {manager}@{version}"
            )),
            "{result}"
        );
        println!(
            "Discovered {manager}@{version} installed with CI=true and no lockfile; its generated lock and dependency survived the prepared snapshot."
        );
    }
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_projects_keep_distinct_manager_versions_in_one_environment() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    for (name, version) in [("first", "10.9.0"), ("second", "10.9.1")] {
        let directory = repo.join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("package.json"), json!({"name":name,"version":"1.0.0","private":true,"packageManager":format!("npm@{version}"),"scripts":{"postinstall":"node test.cjs","test":"node test.cjs","start":"node test.cjs"}}).to_string()).unwrap();
        std::fs::write(directory.join("package-lock.json"),json!({"name":name,"version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":name,"version":"1.0.0","hasInstallScript":true}}}).to_string()).unwrap();
        std::fs::write(directory.join("test.cjs"),format!("const assert=require('node:assert/strict'); const {{execFileSync}}=require('node:child_process'); assert(process.env.npm_config_user_agent.includes('npm/{version}')); assert.equal(execFileSync('npm',['--version'],{{encoding:'utf8'}}).trim(),'{version}'); console.log('verified {name} npm/{version} and nested npm');\n")).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "different project manager pins"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/discovery-monorepo","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 2, "{found}");
    let setup_commands: Vec<_> = projects
        .iter()
        .map(|project| {
            format!(
                "(cd /workspace/{} && {})",
                project["directory"].as_str().unwrap(),
                project["setup"].as_str().unwrap()
            )
        })
        .collect();
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup_commands.join(" && ")}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    // Run both only after both managers are installed, then repeat in reverse
    // order. Each project's nested manager command must retain its own pin.
    let commands: Vec<_> = projects
        .iter()
        .chain(projects.iter().rev())
        .map(|project| {
            format!(
                "(cd /workspace/{} && {} && {})",
                project["directory"].as_str().unwrap(),
                project["test"].as_str().unwrap(),
                project["start"].as_str().unwrap()
            )
        })
        .collect();
    let result = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":setup["id"],"command":commands.join(" && ")}),
    )
    .await;
    assert_eq!(result["status"], "passed", "{result}");
    let output = result["stdout"].as_str().unwrap();
    for marker in [
        "verified first npm/10.9.0 and nested npm",
        "verified second npm/10.9.1 and nested npm",
    ] {
        assert_eq!(output.matches(marker).count(), 4, "{result}");
    }
    println!(
        "Two discovered projects retained npm 10.9.0 and 10.9.1 in one prepared environment; test/start and nested commands passed offline in both execution orders."
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public PyPI access for pytest"]
async fn discovered_python_package_installs_runtime_and_test_dependencies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    let package = repo.join("package");
    let tools = repo.join("tools");
    std::fs::create_dir_all(package.join("src/crow_fixture")).unwrap();
    std::fs::create_dir(&tools).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(
        package.join("pyproject.toml"),
        r#"[build-system]
requires = ["setuptools==80.9.0"]
build-backend = "setuptools.build_meta"
[project]
name = "crow-python-discovery-fixture"
version = "1.0.0"
dependencies = ["humanize==4.13.0"]
[tool.setuptools.packages.find]
where = ["src"]
"#,
    )
    .unwrap();
    std::fs::write(
        package.join("setup.py"),
        "from setuptools import setup\nsetup()\n",
    )
    .unwrap();
    std::fs::write(package.join("requirements.txt"), "pytest==8.4.2\n").unwrap();
    std::fs::write(
        package.join("src/crow_fixture/__init__.py"),
        "import humanize\ndef size(value):\n    return humanize.naturalsize(value, binary=True)\n",
    )
    .unwrap();
    std::fs::write(package.join("test_package.py"), "from crow_fixture import size\ndef test_runtime_dependency():\n    assert size(1024) == '1.0 KiB'\n").unwrap();
    std::fs::write(
        tools.join("pyproject.toml"),
        "[tool.pytest.ini_options]\naddopts='-q'\n",
    )
    .unwrap();
    std::fs::write(tools.join("requirements.txt"), "pytest==8.4.2\n").unwrap();
    std::fs::write(tools.join("test_tools.py"), "import importlib.util\ndef test_separate_tools_environment():\n    assert importlib.util.find_spec('humanize') is None\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "commit",
            "-m",
            "package runtime dependencies and separate test requirements",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/python-installable","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(found["projectCount"], 2, "{found}");
    assert_eq!(projects.len(), 2, "{found}");
    let package = projects
        .iter()
        .find(|project| project["directory"] == "package")
        .unwrap();
    let tools = projects
        .iter()
        .find(|project| project["directory"] == "tools")
        .unwrap();
    assert_eq!(package["manifests"].as_array().unwrap().len(), 3);
    assert!(
        package["setup"]
            .as_str()
            .unwrap()
            .ends_with("-r requirements.txt -e .")
    );
    assert!(
        tools["setup"]
            .as_str()
            .unwrap()
            .ends_with("-r requirements.txt")
    );

    // Reproduce the old requirements-only candidate on the same tracked code.
    // The src-layout package cannot be imported until editable setup runs.
    let old_setup = package["setup"].as_str().unwrap().replace(" -e .", "");
    let old = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":format!("cd /workspace/package && {old_setup}")}),
    )
    .await;
    assert_eq!(old["status"], "passed", "{old}");
    let command = format!(
        "cd /workspace/package && {}",
        package["test"].as_str().unwrap()
    );
    let missing_package = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":old["id"],"command":command}),
    )
    .await;
    assert_eq!(missing_package["status"], "failed", "{missing_package}");
    assert!(
        missing_package["stdout"]
            .as_str()
            .unwrap()
            .contains("No module named 'crow_fixture'"),
        "{missing_package}"
    );

    let setup_commands: Vec<_> = projects
        .iter()
        .map(|project| {
            format!(
                "(cd /workspace/{} && {})",
                project["directory"].as_str().unwrap(),
                project["setup"].as_str().unwrap()
            )
        })
        .collect();
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup_commands.join(" && ")}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    let test_commands: Vec<_> = projects
        .iter()
        .map(|project| {
            format!(
                "(cd /workspace/{} && {})",
                project["directory"].as_str().unwrap(),
                project["test"].as_str().unwrap()
            )
        })
        .collect();
    let result = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":setup["id"],"command":test_commands.join(" && ")}),
    )
    .await;
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(
        result["stdout"]
            .as_str()
            .unwrap()
            .matches("1 passed")
            .count(),
        2,
        "{result}"
    );
    println!(
        "Requirements-only setup reproduced missing src-layout package. Combined discovery installed the package, humanize runtime dependency, and pytest; both package and tool-only project passed offline in separate environments."
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public PyPI access for pytest"]
async fn discovered_python_projects_keep_conflicting_dependencies_separate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    // Build tiny pure-Python wheels inside the sandbox. The same package name
    // has incompatible versions in the two projects, without a build backend.
    std::fs::write(repo.join("build_wheels.py"),r#"import base64, hashlib, io, csv, zipfile
from pathlib import Path
for project, version in [('first','1.0.0'), ('second','2.0.0')]:
    package = 'crow_discovery_dependency'
    info = f'{package}-{version}.dist-info'
    files = {
        f'{package}/__init__.py': f'VERSION = {version!r}\n',
        f'{info}/METADATA': f'Metadata-Version: 2.1\nName: crow-discovery-dependency\nVersion: {version}\n',
        f'{info}/WHEEL': 'Wheel-Version: 1.0\nGenerator: crow-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n',
    }
    record = io.StringIO()
    writer = csv.writer(record, lineterminator='\n')
    for name, body in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(body.encode()).digest()).rstrip(b'=').decode()
        writer.writerow([name, f'sha256={digest}', len(body.encode())])
    writer.writerow([f'{info}/RECORD', '', ''])
    files[f'{info}/RECORD'] = record.getvalue()
    with zipfile.ZipFile(Path(project) / f'{package}-{version}-py3-none-any.whl', 'w') as wheel:
        for name, body in files.items(): wheel.writestr(name, body)
"#).unwrap();
    for (name, version) in [("first", "1.0.0"), ("second", "2.0.0")] {
        let directory = repo.join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("requirements.txt"),
            format!("pytest==8.4.2\n./crow_discovery_dependency-{version}-py3-none-any.whl\n"),
        )
        .unwrap();
        std::fs::write(directory.join("test_dependency.py"),format!("import os, subprocess, sys\nfrom crow_discovery_dependency import VERSION\ndef test_isolated_dependency():\n    assert VERSION == '{version}'\n    assert os.environ['VIRTUAL_ENV'] == sys.prefix\n    nested = subprocess.check_output(['python', '-c', 'from crow_discovery_dependency import VERSION; print(VERSION)'], text=True).strip()\n    assert nested == '{version}'\n")).unwrap();
    }
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "incompatible Python project dependencies"],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/python-discovery","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 2, "{found}");
    assert_ne!(projects[0]["test"], projects[1]["test"]);
    let mut setup_commands = vec!["python3 -I build_wheels.py".to_owned()];
    setup_commands.extend(projects.iter().map(|project| {
        format!(
            "(cd /workspace/{} && {})",
            project["directory"].as_str().unwrap(),
            project["setup"].as_str().unwrap()
        )
    }));
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":setup_commands.join(" && ")}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    let commands: Vec<_> = projects
        .iter()
        .chain(projects.iter().rev())
        .map(|project| {
            format!(
                "(cd /workspace/{} && {})",
                project["directory"].as_str().unwrap(),
                project["test"].as_str().unwrap()
            )
        })
        .collect();
    let result = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":setup["id"],"command":commands.join(" && ")}),
    )
    .await;
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(
        result["stdout"]
            .as_str()
            .unwrap()
            .matches("1 passed")
            .count(),
        4,
        "{result}"
    );
    println!(
        "Two discovered Python projects retained incompatible local dependency versions in one prepared environment; pytest and nested Python imports passed offline in both execution orders."
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_yarn_workspace_inherits_root_setup_and_runs_pnp_child_offline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("packages/app/plugins/foo")).unwrap();
    std::fs::create_dir_all(repo.join("packages/app/tools/helper")).unwrap();
    std::fs::create_dir_all(repo.join("packages/shared")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("package.json"),r#"{"name":"workspace-root","version":"1.0.0","private":true,"packageManager":"yarn@4.9.2","workspaces":["packages/**"]}"#).unwrap();
    std::fs::write(
        repo.join(".yarnrc.yml"),
        "nodeLinker: pnp\nenableGlobalCache: false\nhttpTimeout: 15000\nhttpRetry: 0\n",
    )
    .unwrap();
    std::fs::write(repo.join("packages/app/package.json"),r#"{"name":"app","version":"1.0.0","private":true,"workspaces":["plugins/*"],"dependencies":{"shared":"workspace:*","is-number":"7.0.0"},"scripts":{"test":"node test.cjs"}}"#).unwrap();
    std::fs::write(repo.join("packages/app/plugins/foo/package.json"),r#"{"name":"nested-plugin","version":"1.0.0","private":true,"dependencies":{"shared":"workspace:*","is-number":"7.0.0"},"scripts":{"test":"node test.cjs"}}"#).unwrap();
    std::fs::write(repo.join("packages/app/tools/helper/package.json"),r#"{"name":"helper","version":"1.0.0","private":true,"dependencies":{"shared":"workspace:*","is-number":"7.0.0"},"scripts":{"test":"node test.cjs"}}"#).unwrap();
    std::fs::write(
        repo.join("packages/shared/package.json"),
        r#"{"name":"shared","version":"1.0.0","private":true,"main":"index.cjs"}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/shared/index.cjs"),
        "module.exports = 42;\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/app/test.cjs"),
        r#"const assert = require('node:assert/strict');
const {execFileSync} = require('node:child_process');
assert(process.versions.pnp);
assert.equal(require('shared'), 42);
assert.equal(require('is-number')('42'), true);
assert.equal(require('is-number')('not a number'), false);
for (const variable of ['YARN_HTTP_PROXY', 'YARN_HTTPS_PROXY', 'HTTP_PROXY', 'HTTPS_PROXY', 'http_proxy', 'https_proxy']) {
  assert.equal(process.env[variable], undefined, `${variable} leaked into offline experiment`);
}

assert.equal(execFileSync('yarn', ['--version'], {encoding:'utf8'}).trim(), '4.9.2');
console.log('workspace child used inherited Yarn and PnP dependency offline');
"#,
    ).unwrap();
    std::fs::copy(
        repo.join("packages/app/test.cjs"),
        repo.join("packages/app/plugins/foo/test.cjs"),
    )
    .unwrap();
    std::fs::copy(
        repo.join("packages/app/test.cjs"),
        repo.join("packages/app/tools/helper/test.cjs"),
    )
    .unwrap();
    std::fs::write(
        repo.join("yarn.lock"),
        r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"app@workspace:packages/app":
  version: 0.0.0-use.local
  resolution: "app@workspace:packages/app"
  dependencies:
    is-number: "npm:7.0.0"
    shared: "workspace:*"
  languageName: unknown
  linkType: soft

"helper@workspace:packages/app/tools/helper":
  version: 0.0.0-use.local
  resolution: "helper@workspace:packages/app/tools/helper"
  dependencies:
    is-number: "npm:7.0.0"
    shared: "workspace:*"
  languageName: unknown
  linkType: soft

"is-number@npm:7.0.0":
  version: 7.0.0
  resolution: "is-number@npm:7.0.0"
  checksum: 10c0/b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
  languageName: node
  linkType: hard

"nested-plugin@workspace:packages/app/plugins/foo":
  version: 0.0.0-use.local
  resolution: "nested-plugin@workspace:packages/app/plugins/foo"
  dependencies:
    is-number: "npm:7.0.0"
    shared: "workspace:*"
  languageName: unknown
  linkType: soft

"shared@workspace:*, shared@workspace:packages/shared":
  version: 0.0.0-use.local
  resolution: "shared@workspace:packages/shared"
  languageName: unknown
  linkType: soft

"workspace-root@workspace:.":
  version: 0.0.0-use.local
  resolution: "workspace-root@workspace:."
  languageName: unknown
  linkType: soft
"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "Yarn PnP child workspace fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/yarn-workspace","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let projects = found["projects"].as_array().unwrap();
    let direct = projects
        .iter()
        .find(|project| project["directory"] == "packages/app")
        .unwrap();
    let child = found["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["directory"] == "packages/app/plugins/foo")
        .unwrap();
    assert_eq!(direct["setupDirectory"], ".", "{found}");
    assert_eq!(child["setupDirectory"], ".", "{found}");
    assert_eq!(direct["setup"], child["setup"]);
    assert_eq!(child["packageManager"], "yarn@4.9.2");
    assert!(child["warning"].is_null(), "{child}");
    let helper = projects
        .iter()
        .find(|project| project["directory"] == "packages/app/tools/helper")
        .unwrap();
    assert_eq!(helper["setupDirectory"], ".", "{found}");
    assert_eq!(helper["setup"], child["setup"]);
    assert_eq!(helper["packageManager"], "yarn@4.9.2");
    assert!(helper["warning"].is_null(), "{helper}");
    let setup=call(&exec,"prepare_environment",json!({"revision":"head","setup":format!("cd /workspace/{} && {}",child["setupDirectory"].as_str().unwrap(),child["setup"].as_str().unwrap())})).await;
    assert_eq!(setup["status"], "passed", "{setup}");
    assert!(
        setup["stdout"]
            .as_str()
            .unwrap()
            .contains("A package was added to the project"),
        "Expected a cold remote dependency fetch: {setup}"
    );
    let commands: Vec<_> = [direct, child, helper]
        .into_iter()
        .map(|project| {
            format!(
                "(cd /workspace/{} && {})",
                project["directory"].as_str().unwrap(),
                project["test"].as_str().unwrap()
            )
        })
        .collect();
    let result = call(
        &exec,
        "run_experiment",
        json!({"revision":"head","environment":setup["id"],"command":commands.join(" && ")}),
    )
    .await;
    assert_eq!(result["status"], "passed", "{result}");
    assert_eq!(
        result["stdout"]
            .as_str()
            .unwrap()
            .matches("workspace child used inherited Yarn and PnP dependency offline")
            .count(),
        3,
        "{result}"
    );
    println!(
        "Yarn fetched is-number@7.0.0 with an immutable lock. Direct, nested, and outer-recursive workspaces used remote and local PnP dependencies offline; nested Yarn commands passed and setup proxy settings were absent."
    );
    // Repeating the root's pin or adding a nested lockfile does not make a
    // selected Yarn member independent. Cover own and intermediate settings.
    for directory in ["packages/app", "packages/app/tools/helper"] {
        let path = repo.join(directory).join("package.json");
        let mut package: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        package["packageManager"] = json!("yarn@4.9.2");
        std::fs::write(path, package.to_string()).unwrap();
    }
    std::fs::write(repo.join("packages/app/yarn.lock"), "__metadata:\n  version: 8\n  cacheKey: 10c0\n\n\"app@workspace:.\":\n  version: 0.0.0-use.local\n  resolution: \"app@workspace:.\"\n  languageName: unknown\n  linkType: soft\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "repeat root pin and add nested lockfile"],
    );
    let pinned_commit = git(&repo, &["rev-parse", "HEAD"]);
    let mut pinned_context = context.clone();
    pinned_context["source"]["head"] = json!(pinned_commit);
    let pinned_exec =
        Execution::from_context(&pinned_context, &temp.path().join("pinned-experiments"))
            .unwrap()
            .unwrap();
    let pinned_found = call(
        &pinned_exec,
        "discover_environment",
        json!({"revision":"head"}),
    )
    .await;
    for directory in [
        "packages/app",
        "packages/app/plugins/foo",
        "packages/app/tools/helper",
    ] {
        let project = pinned_found["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|project| project["directory"] == directory)
            .unwrap();
        assert!(
            project["warning"]
                .as_str()
                .unwrap()
                .contains("Nested pins or lockfiles"),
            "{project}"
        );
        for command in ["setup", "test", "start"] {
            assert_eq!(project[command], "", "{project}");
        }
    }
    let root_project = pinned_found["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["directory"] == ".")
        .unwrap();
    let runner = child["test"]
        .as_str()
        .unwrap()
        .strip_suffix(" test")
        .unwrap();
    let actual = call(&pinned_exec, "prepare_environment", json!({"revision":"head","setup":format!("{} && {runner} workspaces list --json",root_project["setup"].as_str().unwrap())})).await;
    assert_eq!(actual["status"], "passed", "{actual}");
    for directory in ["packages/app/plugins/foo", "packages/app/tools/helper"] {
        assert!(
            actual["stdout"]
                .as_str()
                .unwrap()
                .lines()
                .any(|line| serde_json::from_str::<Value>(line)
                    .is_ok_and(|workspace| workspace["location"] == directory)),
            "Yarn must still own {directory}: {actual}"
        );
    }
    println!(
        "Yarn still selected members beneath repeated pins; Crow withheld ambiguous setup/test commands and gave an inspection warning."
    );
}

#[tokio::test]
#[ignore = "requires rootless Podman, the managed toolchain and public npm registry access"]
async fn discovered_yarn_deep_member_uses_root_despite_unselected_intermediate_match() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".crow-data/autonomous-runtime-test");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(&root).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("packages/app/plugins/foo")).unwrap();
    std::fs::create_dir_all(repo.join("packages/shared")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("package.json"), r#"{"name":"root","private":true,"packageManager":"yarn@4.9.2","workspaces":["packages/*/plugins/*","packages/shared"]}"#).unwrap();
    // This intermediate manifest selects the plugin, but Yarn's root does
    // not select the intermediate manifest. Setup must still use the root.
    std::fs::write(
        repo.join("packages/app/package.json"),
        r#"{"name":"app","private":true,"workspaces":["plugins/*"]}"#,
    )
    .unwrap();
    std::fs::write(repo.join("packages/app/plugins/foo/package.json"), r#"{"name":"plugin","private":true,"dependencies":{"shared":"workspace:*"},"scripts":{"test":"node test.cjs"}}"#).unwrap();
    std::fs::write(
        repo.join("packages/shared/package.json"),
        r#"{"name":"shared","private":true,"main":"index.cjs"}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/shared/index.cjs"),
        "module.exports = 42;\n",
    )
    .unwrap();
    std::fs::write(repo.join("packages/app/plugins/foo/test.cjs"), "const assert = require('node:assert/strict'); assert(process.versions.pnp); assert.equal(require('shared'), 42); console.log('direct deep member imported root sibling offline');\n").unwrap();
    std::fs::write(
        repo.join(".yarnrc.yml"),
        "nodeLinker: pnp\nenableGlobalCache: false\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("yarn.lock"),
        r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"plugin@workspace:packages/app/plugins/foo":
  version: 0.0.0-use.local
  resolution: "plugin@workspace:packages/app/plugins/foo"
  dependencies:
    shared: "workspace:*"
  languageName: unknown
  linkType: soft

"root@workspace:.":
  version: 0.0.0-use.local
  resolution: "root@workspace:."
  languageName: unknown
  linkType: soft

"shared@workspace:*, shared@workspace:packages/shared":
  version: 0.0.0-use.local
  resolution: "shared@workspace:packages/shared"
  languageName: unknown
  linkType: soft
"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "Yarn direct deep workspace fixture"],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let config = json!({"podman":std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into()),"automatic":true});
    let context = json!({"root":root,"job":{"repo":"fixture/yarn-deep-workspace","settings":{"execution":config}},"source":{"dir":repo,"head":commit,"base":commit}});
    let exec = Execution::from_context(&context, &temp.path().join("experiments"))
        .unwrap()
        .unwrap();
    let found = call(&exec, "discover_environment", json!({"revision":"head"})).await;
    let plugin = found["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["directory"] == "packages/app/plugins/foo")
        .unwrap();
    assert_eq!(plugin["setupDirectory"], ".", "{found}");
    assert_eq!(plugin["packageManager"], "yarn@4.9.2");
    let setup = call(
        &exec,
        "prepare_environment",
        json!({"revision":"head","setup":plugin["setup"]}),
    )
    .await;
    assert_eq!(setup["status"], "passed", "{setup}");
    let result = call(&exec, "run_experiment", json!({"revision":"head","environment":setup["id"],"command":format!("cd /workspace/packages/app/plugins/foo && {}",plugin["test"].as_str().unwrap())})).await;
    assert_eq!(result["status"], "passed", "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("direct deep member imported root sibling offline"),
        "{result}"
    );
    println!(
        "Yarn directly selected a deep plugin without its intermediate app; Crow installed at the root and the plugin imported its sibling through PnP offline."
    );
}
