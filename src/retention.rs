//! Remove only expired artifacts whose ownership is recorded by the service.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{collections::HashSet, fs, io::ErrorKind, path::Path};

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
fn directory(path: &Path) -> Result<bool> {
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
    let mut warnings = vec![];
    let mut removed = vec![];
    if !directory(root)? {
        return Ok(json!({"removed":removed,"warnings":warnings}));
    }
    let expired: Vec<&Value> = jobs
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
