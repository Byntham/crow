//! Reclaim runtime resources recorded by this worker, never unrelated Podman data.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
    time::Duration,
};

fn receipt_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn image_id(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn image_tag(value: &str) -> bool {
    value
        .strip_prefix("localhost/crow-runtime:")
        .is_some_and(|id| id.len() == 20 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}
async fn podman(
    executable: &str,
    env: &BTreeMap<String, String>,
    args: &[String],
) -> Result<String> {
    Ok(crate::process::run(
        executable,
        args,
        crate::process::RunOptions {
            env: Some(env.clone()),
            timeout: Some(Duration::from_secs(20)),
            max_output: 1024 * 1024,
            ..Default::default()
        },
    )
    .await?
    .stdout)
}
fn remove_temporary(path: &Path, prefix: &str) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
        count += 1;
    }
    Ok(count)
}
fn read_record(path: &Path) -> Result<Value> {
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.is_file() && meta.len() <= 1_048_576,
        "Invalid runtime ownership record: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Invalid runtime ownership record: {}", path.display()))
}

/// Run before retention removes ownership receipts. Active execution locks are skipped.
/// A stopped container is removed only when a matching receipt belongs to a known job.
/// Image removal uses this root's registry and never force-removes an image in use.
/// Preserve `deferredJobs` from evidence retention so later passes can retry cleanup.
pub async fn cleanup_with_images(
    root: &Path,
    executable: &str,
    env: &BTreeMap<String, String>,
    jobs: &[Value],
    protected_images: &[String],
) -> Result<Value> {
    cleanup_at(
        root,
        executable,
        env,
        jobs,
        protected_images,
        crate::util::now(),
    )
    .await
}
async fn cleanup_at(
    root: &Path,
    executable: &str,
    env: &BTreeMap<String, String>,
    jobs: &[Value],
    configured_images: &[String],
    now: i64,
) -> Result<Value> {
    let mut warnings = vec![];
    let mut removed_containers = vec![];
    let mut temporary_files = 0;
    let mut protected_images: HashSet<String> = configured_images
        .iter()
        .filter(|id| image_id(id))
        .cloned()
        .collect();
    let mut containers: Option<HashSet<String>> = None;
    let mut existing_containers = HashSet::new();
    let mut deferred_jobs = HashSet::new();
    for job in jobs {
        let Some(job_id) = job["id"]
            .as_str()
            .filter(|id| crate::retention::valid_id(id))
        else {
            continue;
        };
        let action: Result<()> = async {
            let Some(path) = crate::retention::experiments(root, job_id)? else {
                return Ok(());
            };
            // Preserve every image used by resumable jobs, even when execution is active.
            let mut records = vec![];
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".json").filter(|id| receipt_id(id)) else {
                    continue;
                };
                let record = read_record(&entry.path())?;
                anyhow::ensure!(
                    record["id"] == id,
                    "Runtime receipt ID mismatch for {job_id}/{id}"
                );
                if !matches!(
                    job["state"].as_str(),
                    Some("completed" | "cancelled" | "superseded")
                ) && let Some(image) = record["image"].as_str().filter(|id| image_id(id))
                {
                    protected_images.insert(image.to_owned());
                }
                if record["containerStarted"] != false {
                    records.push((entry.path(), record));
                }
            }
            let Some(_lock) = crate::retention::execution_lock(&path)? else {
                deferred_jobs.insert(job_id.to_owned());
                return Ok(());
            };
            temporary_files += remove_temporary(&path, ".crow-runtime-")?;
            // Export writes beside its destination before atomically publishing it.
            // A killed worker can leave partial snapshots or screenshots here even
            // when a paused review must retain its completed environments.
            for child in ["environments", "artifacts"] {
                let directory = path.join(child);
                if crate::retention::directory(&directory)? {
                    temporary_files += remove_temporary(&directory, ".crow-runtime-")?;
                }
            }
            if !records.is_empty() && containers.is_none() {
                let output = podman(
                    executable,
                    env,
                    &[
                        "ps".into(),
                        "--all".into(),
                        "--format=json".into(),
                        "--filter=name=crow-experiment-".into(),
                    ],
                )
                .await?;
                let inventory: Vec<Value> =
                    serde_json::from_str(&output).context("Invalid Podman container inventory")?;
                existing_containers.extend(
                    inventory
                        .iter()
                        .flat_map(|record| record["Names"].as_array().into_iter().flatten())
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                containers = Some(stopped_containers(&inventory));
            }
            for (receipt_path, _) in records {
                // The experiment may have completed between the inventory read
                // and acquiring its lock. Never overwrite its final evidence
                // with an earlier running receipt during cleanup recovery.
                let mut record = read_record(&receipt_path)?;
                let id = record["id"].as_str().unwrap();
                let name = format!("crow-experiment-{id}");
                if !containers
                    .as_ref()
                    .is_some_and(|values| values.contains(&name))
                {
                    if existing_containers.contains(&name) {
                        deferred_jobs.insert(job_id.to_owned());
                    }
                    continue;
                }
                // Never use --force: a concurrent restart must make this fail safely.
                podman(
                    executable,
                    env,
                    &["rm".into(), "--ignore".into(), name.clone()],
                )
                .await?;
                if record["status"] == "running" {
                    record["status"] = json!("interrupted");
                }
                record["cleanupRecoveredAt"] = json!(now);
                crate::util::atomic(&receipt_path, &record)?;
                removed_containers.push(name);
            }
            Ok(())
        }
        .await;
        if let Err(error) = action {
            deferred_jobs.insert(job_id.to_owned());
            warnings.push(format!("Runtime cleanup for review {job_id}: {error:#}"));
        }
    }
    let mut removed_images = vec![];
    // An unreadable receipt may reference an old image needed for a paused
    // review. Defer image expiration until the inventory is complete again.
    if warnings.is_empty() {
        match cleanup_images(root, executable, env, &protected_images, now).await {
            Ok((images, count)) => {
                removed_images = images;
                temporary_files += count;
            }
            Err(error) => warnings.push(format!("Managed runtime image cleanup: {error:#}")),
        }
    }
    let mut deferred_jobs: Vec<_> = deferred_jobs.into_iter().collect();
    deferred_jobs.sort();
    Ok(
        json!({"containers":removed_containers,"images":removed_images,"temporaryFiles":temporary_files,"deferredJobs":deferred_jobs,"warnings":warnings}),
    )
}
fn stopped_containers(inventory: &[Value]) -> HashSet<String> {
    inventory
        .iter()
        .filter(|container| {
            matches!(
                container["State"].as_str(),
                Some("exited" | "stopped" | "created" | "configured")
            )
        })
        .flat_map(|container| container["Names"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|name| {
            name.strip_prefix("crow-experiment-")
                .is_some_and(receipt_id)
        })
        .map(str::to_owned)
        .collect()
}
async fn cleanup_images(
    root: &Path,
    executable: &str,
    env: &BTreeMap<String, String>,
    protected: &HashSet<String>,
    now: i64,
) -> Result<(Vec<String>, usize)> {
    let cache = root.join("runtime-cache");
    for path in [root.to_owned(), cache.clone()] {
        if !crate::retention::directory(&path)? {
            return Ok((vec![], 0));
        }
    }
    use std::os::unix::fs::OpenOptionsExt;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(cache.join("image.lock"))?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok((vec![], 0)),
        Err(error) => return Err(error.into()),
    }
    let count = remove_temporary(&cache, ".crow-build-")?;
    let registry = cache.join("images");
    if !crate::retention::directory(&registry)? {
        return Ok((vec![], count));
    }
    let current = crate::runtime::image_tag(&cache)?;
    let mut removed = vec![];
    for entry in fs::read_dir(registry)? {
        let entry = entry?;
        let record = read_record(&entry.path())?;
        let tag = record["tag"]
            .as_str()
            .filter(|tag| image_tag(tag))
            .context("Invalid managed image tag")?;
        let id = record["image"]
            .as_str()
            .filter(|id| image_id(id))
            .context("Invalid managed image ID")?;
        let last_used = record["lastUsedAt"]
            .as_i64()
            .context("Invalid managed image access time")?;
        if tag == current
            || protected.contains(id)
            || now.saturating_sub(last_used) < 7 * 86_400_000
        {
            continue;
        }
        // Confirm the tag still identifies the image Crow recorded. A retagged image
        // belongs to its new owner and is never removed by this registry.
        let output = podman(
            executable,
            env,
            &[
                "images".into(),
                "--no-trunc".into(),
                "--format=json".into(),
                tag.to_owned(),
            ],
        )
        .await?;
        let inventory: Vec<Value> =
            serde_json::from_str(&output).context("Invalid Podman image inventory")?;
        if inventory.is_empty() {
            fs::remove_file(entry.path())?;
            continue;
        }
        let matches = inventory.iter().any(|image| {
            image
                .get("Id")
                .or_else(|| image.get("ID"))
                .and_then(Value::as_str)
                .is_some_and(|actual| {
                    actual.strip_prefix("sha256:").unwrap_or(actual)
                        == id.strip_prefix("sha256:").unwrap()
                })
        });
        if !matches {
            continue;
        }
        podman(
            executable,
            env,
            &["image".into(), "rm".into(), tag.to_owned()],
        )
        .await?;
        fs::remove_file(entry.path())?;
        removed.push(tag.to_owned());
    }
    Ok((removed, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn mock(root: &Path) -> (String, BTreeMap<String, String>) {
        use std::os::unix::fs::PermissionsExt;
        let program = root.join("podman");
        fs::write(
            &program,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_DIR/calls"
case "$1" in
 ps) cat "$MOCK_DIR/containers.json" ;;
 images) cat "$MOCK_DIR/images.json" ;;
 rm) test ! -e "$MOCK_DIR/fail-remove" ;;
 image) test ! -e "$MOCK_DIR/fail-remove" ;;
 *) exit 7 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut env: BTreeMap<String, String> = crate::util::host_env().into_iter().collect();
        env.insert("MOCK_DIR".into(), root.to_string_lossy().into_owned());
        (program.to_string_lossy().into_owned(), env)
    }
    #[tokio::test]
    async fn stopped_receipt_owned_containers_and_abandoned_temporaries_are_cleaned() {
        let root = tempfile::tempdir().unwrap();
        let (program, env) = mock(root.path());
        let stopped = "a".repeat(32);
        let running = "b".repeat(32);
        let unrelated = "c".repeat(32);
        let locked = "d".repeat(32);
        let inventory: Vec<_> = [
            (&stopped, "exited"),
            (&running, "running"),
            (&unrelated, "exited"),
            (&locked, "exited"),
        ]
        .into_iter()
        .map(|(id, state)| json!({"Names":[format!("crow-experiment-{id}")],"State":state}))
        .collect();
        file(&root.path().join("containers.json"), &json!(inventory));
        for (job, id, status) in [
            ("done", &stopped, "failed"),
            ("active", &running, "running"),
            ("locked", &locked, "running"),
        ] {
            file(
                &root
                    .path()
                    .join(format!("reviews/{job}/experiments/{id}.json")),
                &json!({"id":id,"status":status,"cleanupError":"deliberate prior failure"}),
            );
            let dir = root.path().join(format!("reviews/{job}/experiments"));
            fs::create_dir(dir.join(".crow-runtime-downloads-orphan")).unwrap();
            fs::write(dir.join(".crow-runtime-archive"), "large archive").unwrap();
            fs::write(dir.join("screenshot.png"), "evidence").unwrap();
        }
        let lock =
            crate::retention::execution_lock(&root.path().join("reviews/locked/experiments"))
                .unwrap()
                .unwrap();
        let jobs = [
            json!({"id":"done","state":"completed"}),
            json!({"id":"active","state":"reviewing"}),
            json!({"id":"locked","state":"reviewing"}),
        ];
        let result = cleanup_at(root.path(), &program, &env, &jobs, &[], 100)
            .await
            .unwrap();
        assert_eq!(
            result["containers"],
            json!([format!("crow-experiment-{stopped}")])
        );
        assert_eq!(result["warnings"], json!([]));
        assert_eq!(result["temporaryFiles"], 4);
        assert!(
            root.path()
                .join("reviews/locked/experiments/.crow-runtime-archive")
                .exists()
        );
        assert!(
            root.path()
                .join("reviews/done/experiments/screenshot.png")
                .exists()
        );
        let calls = fs::read_to_string(root.path().join("calls")).unwrap();
        assert!(!calls.contains("--force"));
        assert!(!calls.contains(&format!("rm --ignore crow-experiment-{running}")));
        assert!(!calls.contains(&format!("rm --ignore crow-experiment-{unrelated}")));
        drop(lock);
    }
    #[tokio::test]
    async fn paused_review_export_temporaries_are_removed_without_touching_active_or_foreign_files()
    {
        let root = tempfile::tempdir().unwrap();
        let (program, env) = mock(root.path());
        for job in ["paused", "locked", "foreign"] {
            let experiments = root.path().join(format!("reviews/{job}/experiments"));
            for child in ["", "environments", "artifacts"] {
                let directory = experiments.join(child);
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join(".crow-runtime-partial"), "partial export").unwrap();
                fs::write(directory.join("unrelated.tmp"), "foreign file").unwrap();
            }
            fs::write(
                experiments.join("environments/prepared.tar"),
                "prepared environment",
            )
            .unwrap();
            fs::write(
                experiments.join("artifacts/screenshot.png"),
                "saved screenshot",
            )
            .unwrap();
        }
        let lock =
            crate::retention::execution_lock(&root.path().join("reviews/locked/experiments"))
                .unwrap()
                .unwrap();
        let jobs = [
            json!({"id":"paused","state":"paused"}),
            json!({"id":"locked","state":"reviewing"}),
        ];
        let result = cleanup_at(root.path(), &program, &env, &jobs, &[], 100)
            .await
            .unwrap();
        assert_eq!(result["warnings"], json!([]));
        assert_eq!(result["temporaryFiles"], 3);
        assert_eq!(result["deferredJobs"], json!(["locked"]));
        for job in ["paused", "locked", "foreign"] {
            let experiments = root.path().join(format!("reviews/{job}/experiments"));
            for child in ["", "environments", "artifacts"] {
                let directory = experiments.join(child);
                assert_eq!(
                    directory.join(".crow-runtime-partial").exists(),
                    job != "paused"
                );
                assert!(directory.join("unrelated.tmp").exists());
            }
            assert!(experiments.join("environments/prepared.tar").exists());
            assert!(experiments.join("artifacts/screenshot.png").exists());
        }
        assert!(!root.path().join("calls").exists());
        drop(lock);
    }

    #[tokio::test]
    async fn export_cleanup_rejects_linked_directories_and_preserves_external_files() {
        for child in ["environments", "artifacts"] {
            let root = tempfile::tempdir().unwrap();
            let (program, env) = mock(root.path());
            let external = tempfile::tempdir().unwrap();
            fs::write(
                external.path().join(".crow-runtime-partial"),
                "foreign export",
            )
            .unwrap();
            let experiments = root.path().join("reviews/paused/experiments");
            fs::create_dir_all(&experiments).unwrap();
            std::os::unix::fs::symlink(external.path(), experiments.join(child)).unwrap();
            let result = cleanup_at(
                root.path(),
                &program,
                &env,
                &[json!({"id":"paused","state":"paused"})],
                &[],
                100,
            )
            .await
            .unwrap();
            assert_eq!(result["temporaryFiles"], 0);
            assert_eq!(result["deferredJobs"], json!(["paused"]));
            assert!(result["warnings"][0].as_str().unwrap().contains(child));
            assert!(external.path().join(".crow-runtime-partial").exists());
            assert!(!root.path().join("calls").exists());
        }
    }

    #[tokio::test]
    async fn cleanup_failures_identify_review_and_leave_receipt_for_retry() {
        let root = tempfile::tempdir().unwrap();
        let (program, env) = mock(root.path());
        let id = "a".repeat(32);
        file(
            &root.path().join("containers.json"),
            &json!([{"Names":[format!("crow-experiment-{id}")],"State":"exited"}]),
        );
        file(
            &root
                .path()
                .join(format!("reviews/done/experiments/{id}.json")),
            &json!({"id":id,"status":"running"}),
        );
        fs::write(root.path().join("fail-remove"), "fail").unwrap();
        let jobs = [json!({"id":"done","state":"completed"})];
        let result = cleanup_at(root.path(), &program, &env, &jobs, &[], 100)
            .await
            .unwrap();
        assert!(
            result["warnings"][0]
                .as_str()
                .unwrap()
                .contains("review done")
        );
        assert_eq!(result["containers"], json!([]));
        fs::remove_file(root.path().join("fail-remove")).unwrap();
        let result = cleanup_at(root.path(), &program, &env, &jobs, &[], 101)
            .await
            .unwrap();
        assert_eq!(
            result["containers"].as_array().unwrap().len(),
            1,
            "{result}"
        );
        let record = read_record(
            &root
                .path()
                .join(format!("reviews/done/experiments/{id}.json")),
        )
        .unwrap();
        assert_eq!(record["status"], "interrupted");
        assert_eq!(record["cleanupRecoveredAt"], 101);
    }
    #[tokio::test]
    async fn old_images_require_registry_identity_and_preserve_current_recent_and_paused() {
        let root = tempfile::tempdir().unwrap();
        let (program, env) = mock(root.path());
        let cache = root.path().join("runtime-cache");
        fs::create_dir_all(cache.join("images")).unwrap();
        let current = crate::runtime::image_tag(&cache).unwrap();
        let now = 30 * 86_400_000;
        let image = format!("sha256:{}", "a".repeat(64));
        let protected = format!("sha256:{}", "b".repeat(64));
        let old_tag = format!("localhost/crow-runtime:{}", "a".repeat(20));
        for (name, tag, id, last_used) in [
            ("old", old_tag.clone(), image.clone(), 0),
            ("current", current.clone(), image.clone(), 0),
            (
                "recent",
                format!("localhost/crow-runtime:{}", "c".repeat(20)),
                image.clone(),
                now,
            ),
            (
                "paused",
                format!("localhost/crow-runtime:{}", "d".repeat(20)),
                protected.clone(),
                0,
            ),
            (
                "retagged",
                format!("localhost/crow-runtime:{}", "e".repeat(20)),
                format!("sha256:{}", "f".repeat(64)),
                0,
            ),
        ] {
            file(
                &cache.join(format!("images/{name}.json")),
                &json!({"tag":tag,"image":id,"lastUsedAt":last_used}),
            );
        }
        file(&root.path().join("images.json"), &json!([{"Id":image}]));
        file(&root.path().join("containers.json"), &json!([]));
        let receipt = "a".repeat(32);
        file(
            &root
                .path()
                .join(format!("reviews/paused/experiments/{receipt}.json")),
            &json!({"id":receipt,"status":"passed","image":protected}),
        );
        fs::create_dir(cache.join(".crow-build-orphan")).unwrap();
        let jobs = [json!({"id":"paused","state":"paused"})];
        let result = cleanup_at(root.path(), &program, &env, &jobs, &[], now)
            .await
            .unwrap();
        assert_eq!(result["warnings"], json!([]));
        assert_eq!(result["images"], json!([old_tag]));
        assert_eq!(result["temporaryFiles"], 1);
        assert!(!cache.join("images/old.json").exists());
        for name in ["current", "recent", "paused", "retagged"] {
            assert!(cache.join(format!("images/{name}.json")).exists());
        }
        let calls = fs::read_to_string(root.path().join("calls")).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("image rm"))
                .count(),
            1
        );
        assert!(!calls.contains("prune"));
        assert!(!calls.contains("--force"));
    }
    #[test]
    fn separate_worker_roots_have_separate_image_tags() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_ne!(
            crate::runtime::image_tag(first.path()).unwrap(),
            crate::runtime::image_tag(second.path()).unwrap()
        );
    }
    #[tokio::test]
    #[ignore = "requires rootless Podman and CROW_TEST_IMAGE loaded locally"]
    async fn real_podman_cleanup_preserves_running_foreign_and_current_resources() {
        let root = tempfile::tempdir().unwrap();
        let executable = std::env::var("CROW_TEST_PODMAN").unwrap_or("podman".into());
        let image = std::env::var("CROW_TEST_IMAGE")
            .expect("set CROW_TEST_IMAGE to a local immutable image ID");
        let env: BTreeMap<String, String> = crate::util::host_env().into_iter().collect();
        let stopped = hex::encode(rand::random::<[u8; 16]>());
        let running = hex::encode(rand::random::<[u8; 16]>());
        let foreign = hex::encode(rand::random::<[u8; 16]>());
        let cache = root.path().join("runtime-cache");
        fs::create_dir_all(cache.join("images")).unwrap();
        let current = crate::runtime::image_tag(&cache).unwrap();
        let old = format!("localhost/crow-runtime:{}", &stopped[..20]);
        let foreign_tag = format!("localhost/crow-cleanup-foreign:{}", &foreign[..20]);
        struct Resources {
            executable: String,
            names: Vec<String>,
            tags: Vec<String>,
        }
        impl Drop for Resources {
            fn drop(&mut self) {
                for name in &self.names {
                    let _ = std::process::Command::new(&self.executable)
                        .args(["rm", "--force", "--ignore", name])
                        .output();
                }
                for tag in &self.tags {
                    let _ = std::process::Command::new(&self.executable)
                        .args(["image", "rm", tag])
                        .output();
                }
            }
        }
        let names: Vec<_> = [&stopped, &running, &foreign]
            .into_iter()
            .map(|id| format!("crow-experiment-{id}"))
            .collect();
        let _guard = Resources {
            executable: executable.clone(),
            names: names.clone(),
            tags: vec![old.clone(), current.clone(), foreign_tag.clone()],
        };
        for tag in [&old, &current, &foreign_tag] {
            podman(
                &executable,
                &env,
                &["tag".into(), image.clone(), tag.to_string()],
            )
            .await
            .unwrap();
        }
        for (index, name) in names.iter().enumerate() {
            let args: Vec<String> = if index == 1 {
                [
                    "run",
                    "--detach",
                    "--network=none",
                    "--read-only",
                    "--cap-drop=ALL",
                    "--name",
                    name,
                    &image,
                    "sleep",
                    "60",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            } else {
                ["create", "--network=none", "--name", name, &image, "true"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            };
            podman(&executable, &env, &args).await.unwrap();
        }
        for (job, id) in [("done", &stopped), ("running", &running)] {
            file(
                &root
                    .path()
                    .join(format!("reviews/{job}/experiments/{id}.json")),
                &json!({"id":id,"status":"running"}),
            );
        }
        for (name, tag) in [("old", &old), ("current", &current)] {
            file(
                &cache.join(format!("images/{name}.json")),
                &json!({"tag":tag,"image":image,"lastUsedAt":0}),
            );
        }
        let jobs = [
            json!({"id":"done","state":"completed"}),
            json!({"id":"running","state":"reviewing"}),
        ];
        let result = cleanup_with_images(root.path(), &executable, &env, &jobs, &[])
            .await
            .unwrap();
        assert_eq!(result["warnings"], json!([]), "{result}");
        assert_eq!(result["containers"], json!([names[0]]), "{result}");
        assert_eq!(result["images"], json!([old]), "{result}");
        let inventory: Vec<Value> = serde_json::from_str(
            &podman(
                &executable,
                &env,
                &["ps".into(), "--all".into(), "--format=json".into()],
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let remaining: HashSet<_> = inventory
            .iter()
            .flat_map(|value| value["Names"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        assert!(!remaining.contains(names[0].as_str()));
        assert!(remaining.contains(names[1].as_str()));
        assert!(remaining.contains(names[2].as_str()));
        for tag in [&current, &foreign_tag] {
            podman(
                &executable,
                &env,
                &["image".into(), "inspect".into(), tag.to_string()],
            )
            .await
            .unwrap();
        }
        println!("Runtime cleanup verified: {result}");
    }
}
