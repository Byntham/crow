//! Release terminal runtime snapshots and expire service-owned review evidence.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(crate) fn valid_session(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
pub(crate) fn directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(true),
        Ok(_) => bail!(
            "Retention skipped non-directory or linked path: {}",
            path.display()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn experiments(root: &Path, id: &str) -> Result<Option<PathBuf>> {
    if !valid_id(id) {
        bail!("Invalid review ID for runtime cleanup");
    }
    let path = root.join("reviews").join(id).join("experiments");
    for parent in [
        root.to_owned(),
        root.join("reviews"),
        root.join("reviews").join(id),
        path.clone(),
    ] {
        if !directory(&parent)? {
            return Ok(None);
        }
    }
    Ok(Some(path))
}
/// Share the same lock as MCP execution; cleanup must not remove a live workspace.
pub(crate) fn execution_lock(path: &Path) -> Result<Option<Lock>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("execution.lock"))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(Lock(file))),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Explicit unlock also releases transient fork-inherited descriptor copies.
/// Closing only the parent's descriptor can leave a flock held until another
/// thread's newly spawned subprocess reaches exec and closes its copy.
pub(crate) struct Lock(pub(crate) fs::File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
/// Release bulky snapshots as soon as a review is terminal, while preserving evidence.
/// Callers must mark their locally active jobs as nonterminal until cancellation finishes.
pub fn cleanup_runtime(root: &Path, jobs: &[Value]) -> Result<Value> {
    let mut removed = vec![];
    let mut warnings = vec![];
    for job in jobs {
        if !matches!(
            job["state"].as_str(),
            Some("completed" | "superseded" | "cancelled")
        ) {
            continue;
        }
        let Some(id) = job["id"].as_str().filter(|id| valid_id(id)) else {
            continue;
        };
        let action = || -> Result<bool> {
            let Some(path) = experiments(root, id)? else {
                return Ok(false);
            };
            let Some(_lock) = execution_lock(&path)? else {
                return Ok(false);
            };
            let environments = path.join("environments");
            if !directory(&environments)? {
                return Ok(false);
            }
            fs::remove_dir_all(environments)?;
            Ok(true)
        };
        match action() {
            Ok(true) => removed.push(id.to_owned()),
            Ok(false) => (),
            Err(error) => warnings.push(error.to_string()),
        }
    }
    Ok(json!({"removed":removed,"warnings":warnings}))
}
fn session(job: &Value) -> Option<&str> {
    job["session"]
        .as_str()
        .or_else(|| job["session"]["id"].as_str())
}
fn children(root: &Path, id: &str, warnings: &mut Vec<String>) -> Result<Vec<String>> {
    let review = root.join("reviews").join(id);
    for path in [root.join("reviews"), review.clone(), review.join("tasks")] {
        if !directory(&path)? {
            return Ok(vec![]);
        }
    }
    let mut found = vec![];
    for entry in fs::read_dir(review.join("tasks"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_file() || !name.strip_suffix(".json").is_some_and(valid_id) {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_file() || meta.len() > 1_048_576 {
            continue;
        }
        match fs::read(entry.path())
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<Value>(&b)?))
        {
            Ok(record) => {
                if let Some(id) = record["session"].as_str().filter(|s| valid_session(s)) {
                    found.push(id.into());
                }
            }
            Err(e) => warnings.push(format!("Cannot read delegated session record {name}: {e}")),
        }
    }
    Ok(found)
}
fn walk(path: &Path, sessions: &HashSet<String>) -> Result<()> {
    if !directory(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk(&entry.path(), sessions)?;
        } else if kind.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("rollout-")
                && name.ends_with(".jsonl")
                && name.len() >= 42
                && let Some(id) = name.get(name.len() - 42..name.len() - 6)
                && sessions.contains(id)
            {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}
/// Configuration accepts `retentionDays`, with the same seven-day default as the service.
pub fn cleanup(root: &Path, config: &Value, jobs: &[Value]) -> Result<Value> {
    let days = match config.get("retentionDays") {
        Some(value) => value.as_f64().ok_or_else(|| {
            anyhow::anyhow!("Retention days and current time must be finite nonnegative values")
        })?,
        None => 7.0,
    };
    cleanup_at(root, jobs, days, crate::util::now() as f64)
}
pub fn cleanup_at(root: &Path, jobs: &[Value], days: f64, now: f64) -> Result<Value> {
    if !days.is_finite() || days < 0.0 || !now.is_finite() || now < 0.0 {
        bail!("Retention days and current time must be finite nonnegative values");
    }
    let runtime = cleanup_runtime(root, jobs)?;
    let mut warnings: Vec<String> = serde_json::from_value(runtime["warnings"].clone())?;
    let mut removed = vec![];
    if !directory(root)? {
        return Ok(json!({"removed":removed,"warnings":warnings}));
    }
    let mut expired: Vec<&Value> = jobs
        .iter()
        .filter(|j| {
            valid_id(j["id"].as_str().unwrap_or(""))
                && matches!(
                    j["state"].as_str(),
                    Some("completed" | "superseded" | "cancelled")
                )
                && j["updatedAt"]
                    .as_f64()
                    .is_some_and(|t| t <= now - days * 86_400_000.0)
        })
        .collect();
    // Also protect full evidence expiration against another local MCP process.
    // Keep these locks until all corresponding directories have been removed.
    let mut execution_locks = vec![];
    expired.retain(|job| {
        let id = job["id"].as_str().unwrap();
        match experiments(root, id) {
            Ok(Some(path)) => match execution_lock(&path) {
                Ok(Some(lock)) => {
                    execution_locks.push(lock);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    warnings.push(error.to_string());
                    false
                }
            },
            // Existing retention can safely unlink invalid leaf paths without
            // traversing them, and reports unsafe parents separately below.
            Ok(None) | Err(_) => true,
        }
    });
    let expired_ids: HashSet<&str> = expired.iter().filter_map(|j| j["id"].as_str()).collect();
    let mut retained: HashSet<String> = jobs
        .iter()
        .filter(|j| !expired_ids.contains(j["id"].as_str().unwrap_or("")))
        .filter_map(session)
        .map(str::to_owned)
        .collect();
    let mut sessions: HashSet<String> = expired
        .iter()
        .filter_map(|j| session(j))
        .filter(|s| valid_session(s))
        .map(str::to_owned)
        .collect();
    for job in jobs {
        let Some(id) = job["id"].as_str().filter(|s| valid_id(s)) else {
            continue;
        };
        match children(root, id, &mut warnings) {
            Ok(ids) => {
                if expired_ids.contains(id) {
                    sessions.extend(ids);
                } else {
                    retained.extend(ids);
                }
            }
            Err(e) => warnings.push(e.to_string()),
        }
    }
    sessions.retain(|s| !retained.contains(s));
    for job in expired {
        let id = job["id"].as_str().unwrap();
        let mut failed = false;
        for (folder, name) in [
            ("sources", id.to_owned()),
            ("reviews", id.to_owned()),
            ("reports", format!("{id}.json")),
            ("logs", format!("{id}.log")),
        ] {
            let action = || -> Result<()> {
                let parent = root.join(folder);
                if !directory(&parent)? {
                    return Ok(());
                }
                let path = parent.join(name);
                match fs::symlink_metadata(&path) {
                    Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(path)?,
                    Ok(_) => fs::remove_file(path)?,
                    Err(e) if e.kind() == ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            };
            if let Err(e) = action() {
                failed = true;
                warnings.push(e.to_string());
            }
        }
        if !failed {
            removed.push(id.to_owned());
        }
    }
    if !sessions.is_empty() {
        let action = || -> Result<()> {
            let codex = root.join("codex");
            if directory(&codex)? {
                walk(&codex.join("sessions"), &sessions)?;
                walk(&codex.join("archived_sessions"), &sessions)?;
            }
            Ok(())
        };
        if let Err(e) = action() {
            warnings.push(e.to_string());
        }
    }
    let mut seen = HashSet::new();
    warnings.retain(|s| seen.insert(s.clone()));
    Ok(json!({"removed":removed,"warnings":warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "11111111-2222-3333-4444-555555555555";
    fn file(root: &Path, path: &str, body: &str) {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    #[test]
    fn finished_cleanup_unlocks_even_with_an_inherited_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let lock = execution_lock(root.path()).unwrap().unwrap();
        // dup shares the same open file description as an inherited fork fd.
        let inherited = lock.0.try_clone().unwrap();
        assert!(execution_lock(root.path()).unwrap().is_none());
        drop(lock);
        assert!(execution_lock(root.path()).unwrap().is_some());
        drop(inherited);
    }
    #[test]
    fn terminal_snapshots_are_released_while_evidence_and_live_work_survive() {
        let root = tempfile::tempdir().unwrap();
        let jobs: Vec<_> = [
            ("done", "completed"),
            ("cancelled", "cancelled"),
            ("superseded", "superseded"),
            ("paused", "paused"),
            ("running", "reviewing"),
            ("locked", "completed"),
        ]
        .into_iter()
        .map(|(id, state)| {
            file(
                root.path(),
                &format!("reviews/{id}/experiments/environments/snapshot.tar"),
                "dependencies",
            );
            file(
                root.path(),
                &format!("reviews/{id}/experiments/receipt.json"),
                "evidence",
            );
            file(
                root.path(),
                &format!("reviews/{id}/experiments/shot.png"),
                "screenshot",
            );
            json!({"id":id,"state":state,"updatedAt":1000})
        })
        .collect();
        let lock = execution_lock(&root.path().join("reviews/locked/experiments"))
            .unwrap()
            .unwrap();
        let result = cleanup_runtime(root.path(), &jobs).unwrap();
        assert_eq!(
            result,
            json!({"removed":["done","cancelled","superseded"],"warnings":[]})
        );
        let expired_locked = [json!({"id":"locked","state":"completed","updatedAt":0})];
        assert_eq!(
            cleanup_at(root.path(), &expired_locked, 0.0, 1.0).unwrap()["removed"],
            json!([])
        );
        assert!(
            root.path()
                .join("reviews/locked/experiments/receipt.json")
                .exists()
        );
        for job in &jobs {
            let id = job["id"].as_str().unwrap();
            let experiments = root.path().join(format!("reviews/{id}/experiments"));
            assert!(experiments.join("receipt.json").exists());
            assert!(experiments.join("shot.png").exists());
            assert_eq!(
                experiments.join("environments").exists(),
                ["paused", "running", "locked"].contains(&id)
            );
        }
        drop(lock);
        assert_eq!(
            cleanup_runtime(root.path(), &jobs).unwrap()["removed"],
            json!(["locked"])
        );
    }
    #[test]
    fn runtime_cleanup_rejects_linked_snapshot_and_lock_paths() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        file(outside.path(), "valuable", "keep");
        let dir = root.path().join("reviews/done/experiments");
        fs::create_dir_all(&dir).unwrap();
        symlink(outside.path(), dir.join("environments")).unwrap();
        let jobs = [json!({"id":"done","state":"completed"})];
        assert_eq!(
            cleanup_runtime(root.path(), &jobs).unwrap()["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::remove_file(dir.join("execution.lock")).unwrap();
        symlink(outside.path().join("valuable"), dir.join("execution.lock")).unwrap();
        assert_eq!(
            cleanup_runtime(root.path(), &jobs).unwrap()["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("valuable")).unwrap(),
            "keep"
        );
    }
    #[test]
    fn only_terminal_owned_artifacts_expire() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for id in ["done", "paused", "fresh", "unknown"] {
            file(root, &format!("reviews/{id}/file"), "keep");
        }
        let rollout = format!("codex/sessions/2026/rollout-date-{SESSION}.jsonl");
        file(root, &rollout, "owned");
        file(root, "codex/auth.json", "keep");
        let jobs = vec![
            json!({"id":"done","state":"completed","updatedAt":0,"session":SESSION}),
            json!({"id":"paused","state":"paused","updatedAt":0}),
            json!({"id":"fresh","state":"completed","updatedAt":1e12}),
            json!({"id":"unknown","state":"completed"}),
        ];
        let r = cleanup_at(root, &jobs, 7.0, 1e10).unwrap();
        assert_eq!(r, json!({"removed":["done"],"warnings":[]}));
        assert!(!root.join(rollout).exists());
        assert!(root.join("codex/auth.json").exists());
        for id in ["paused", "fresh", "unknown"] {
            assert!(root.join(format!("reviews/{id}/file")).exists());
        }
        assert!(cleanup_at(root, &[], -1.0, 1.0).is_err());
    }
    #[test]
    fn delegated_sessions_respect_live_references() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let other = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        for (job, id, name) in [
            ("done", SESSION, "a"),
            ("done", other, "b"),
            ("paused", other, "c"),
        ] {
            file(
                root,
                &format!("reviews/{job}/tasks/{name}.json"),
                &json!({"session":id}).to_string(),
            );
            file(
                root,
                &format!("codex/sessions/rollout-date-{id}.jsonl"),
                "keep",
            );
        }
        cleanup_at(
            root,
            &[
                json!({"id":"done","state":"completed","updatedAt":0}),
                json!({"id":"paused","state":"paused","updatedAt":0}),
            ],
            0.0,
            1.0,
        )
        .unwrap();
        assert!(
            !root
                .join(format!("codex/sessions/rollout-date-{SESSION}.jsonl"))
                .exists()
        );
        assert!(
            root.join(format!("codex/sessions/rollout-date-{other}.jsonl"))
                .exists()
        );
    }
    #[cfg(unix)]
    #[test]
    fn linked_codex_home_and_live_sessions_are_preserved() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = format!("sessions/rollout-date-{SESSION}.jsonl");
        file(outside.path(), &path, "keep");
        symlink(outside.path(), root.path().join("codex")).unwrap();
        let done = json!({"id":"done","state":"superseded","updatedAt":0,"session":{"id":SESSION}});
        let result = cleanup_at(root.path(), std::slice::from_ref(&done), 0.0, 1.0).unwrap();
        assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
        assert!(outside.path().join(&path).exists());
        fs::remove_file(root.path().join("codex")).unwrap();
        file(root.path(), &format!("codex/{path}"), "keep");
        let paused = json!({"id":"paused","state":"paused","updatedAt":0,"session":SESSION});
        cleanup_at(root.path(), &[done, paused], 0.0, 1.0).unwrap();
        assert!(root.path().join(format!("codex/{path}")).exists());
        assert!(cleanup(root.path(), &json!({"retentionDays":"invalid"}), &[]).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn never_follows_links_or_traversal_ids() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        file(outside.path(), "valuable", "keep");
        fs::create_dir(t.path().join("sources")).unwrap();
        symlink(outside.path(), t.path().join("sources/done")).unwrap();
        symlink(outside.path(), t.path().join("reviews")).unwrap();
        let r = cleanup_at(
            t.path(),
            &[
                json!({"id":"done","state":"cancelled","updatedAt":0}),
                json!({"id":"../valuable","state":"completed","updatedAt":0}),
            ],
            0.0,
            1.0,
        )
        .unwrap();
        assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
        assert!(outside.path().join("valuable").exists());
        assert!(fs::symlink_metadata(t.path().join("sources/done")).is_err());
    }
}
