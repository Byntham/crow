//! Bounded package-download archives. Prepared workspaces stay local to a review.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const ARCHIVE_LIMIT: u64 = 512 * 1024 * 1024;
const CACHE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(7 * 86400);

pub struct Plan {
    pub key: String,
    pub pins: Value,
}
/// Only pinned Git metadata chooses the cache, never files modified by setup hooks.
pub async fn plan(
    source: &Value,
    revision: &str,
    repository: &str,
    scope: &str,
    image: &str,
) -> Result<Option<Plan>> {
    let commit =
        crate::inspection::revision(source[revision].as_str().context("Missing revision")?)?;
    let dir = Path::new(source["dir"].as_str().context("Missing source directory")?);
    let tree = crate::inspection::git(
        dir,
        &["ls-tree".into(), "-r".into(), "-z".into(), commit.into()],
    )
    .await?;
    let mut fingerprints = Vec::new();
    let mut pins = json!({"npm":false,"cargo":{},"go":{}});
    let mut locks = Vec::new();
    for item in tree.split('\0') {
        let Some((meta, path)) = item.split_once('\t') else {
            continue;
        };
        // Respect repositories that deliberately track the usual cache location.
        // Optional cache directories must never change how pinned source restores.
        if path == ".crow-home" || path.starts_with(".crow-home/") {
            return Ok(None);
        }
        if !meta.starts_with("100644 blob ") && !meta.starts_with("100755 blob ") {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if [
            "package.json",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "Cargo.toml",
            "Cargo.lock",
            "go.mod",
            "go.sum",
        ]
        .contains(&name)
        {
            fingerprints.push(item.to_owned());
        }
        if ["package-lock.json", "npm-shrinkwrap.json"].contains(&name) {
            pins["npm"] = json!(true);
        }
        if ["Cargo.lock", "go.sum"].contains(&name) {
            locks.push((name, path));
        }
    }
    // Large or unreadable lockfiles simply disable caching for that ecosystem.
    for (name, path) in locks.into_iter().take(256) {
        let Ok(body) = crate::inspection::read_blob(source, path, Some(commit)).await else {
            continue;
        };
        if name == "Cargo.lock" {
            let Ok(lock) = toml::from_str::<toml::Value>(&body) else {
                continue;
            };
            for package in lock
                .get("package")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
                    continue;
                };
                let Some(version) = package.get("version").and_then(toml::Value::as_str) else {
                    continue;
                };
                let Some(checksum) = package.get("checksum").and_then(toml::Value::as_str) else {
                    continue;
                };
                if !safe_component(name)
                    || !safe_component(version)
                    || checksum.len() != 64
                    || !checksum.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    continue;
                }
                let key = format!("{name}-{version}.crate");
                if pins["cargo"].get(&key).is_none() {
                    pins["cargo"][&key] = json!([]);
                }
                pins["cargo"][&key]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(checksum.to_lowercase()));
            }
        } else {
            for line in body.lines() {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() != 3 || !parts[2].starts_with("h1:") {
                    continue;
                }
                let module = parts[0];
                let (version, extension) = parts[1]
                    .strip_suffix("/go.mod")
                    .map_or((parts[1], "zip"), |v| (v, "mod"));
                if !module.split('/').all(safe_component) || !safe_component(version) {
                    continue;
                }
                let escaped = |s: &str| {
                    s.chars()
                        .flat_map(|c| {
                            if c.is_ascii_uppercase() {
                                vec!['!', c.to_ascii_lowercase()]
                            } else {
                                vec![c]
                            }
                        })
                        .collect::<String>()
                };
                pins["go"][format!("{}/@v/{}.{}", escaped(module), escaped(version), extension)] =
                    json!(parts[2]);
            }
        }
    }
    if pins["npm"] != true
        && pins["cargo"].as_object().unwrap().is_empty()
        && pins["go"].as_object().unwrap().is_empty()
    {
        return Ok(None);
    }
    if pins.to_string().len() > 64000 {
        return Ok(None);
    }
    fingerprints.sort();
    let key = crate::runtime::fingerprint(&[
        repository,
        scope,
        revision,
        image,
        &fingerprints.join("\0"),
        "verified-downloads-v1",
    ]);
    Ok(Some(Plan { key, pins }))
}
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+~".contains(&b))
}
fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
}
struct CacheLock(fs::File);
impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
fn lock(root: &Path) -> Result<CacheLock> {
    crate::util::private_dir(root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("packages.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(CacheLock(file))
}
fn files(root: &Path) -> Result<Vec<(SystemTime, u64, PathBuf)>> {
    let mut result = Vec::new();
    let dir = root.join("packages");
    crate::util::private_dir(&dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.strip_suffix(".tar").is_some_and(valid_key) {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Package cache entry is not a regular file"
        );
        result.push((meta.modified()?, meta.len(), entry.path()));
    }
    result.sort_by_key(|(time, _, _)| *time);
    Ok(result)
}
fn prune_locked(root: &Path, allowance: u64, now: SystemTime) -> Result<u64> {
    let entries = files(root)?;
    let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
    let mut removed = 0;
    for (time, size, path) in entries {
        if total > allowance
            || size > ARCHIVE_LIMIT
            || now.duration_since(time).unwrap_or_default() > MAX_AGE
        {
            fs::remove_file(path)?;
            total -= size;
            removed += size;
        }
    }
    Ok(removed)
}
pub fn maintain(root: &Path) -> Result<u64> {
    let _lock = lock(root)?;
    let mut removed = prune_locked(root, CACHE_LIMIT, SystemTime::now())?;
    // Older versions saved entire workspaces here. They are no longer reusable.
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.strip_suffix(".tar").is_some_and(valid_key) && entry.file_type()?.is_file() {
            removed += entry.metadata()?.len();
            fs::remove_file(entry.path())?;
        }
    }
    // Interrupted inserts are only ours, and no writer can be active under this lock.
    for entry in fs::read_dir(root.join("packages"))? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".crow-package-")
            && entry.file_type()?.is_file()
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(removed)
}
pub fn load(root: &Path, key: &str, target: &Path) -> Result<bool> {
    ensure!(valid_key(key), "Invalid package cache key");
    let _lock = lock(root)?;
    let source = root.join("packages").join(format!("{key}.tar"));
    let meta = match fs::symlink_metadata(&source) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= ARCHIVE_LIMIT,
        "Invalid package cache archive"
    );
    if meta.modified()?.elapsed().unwrap_or_default() > MAX_AGE {
        fs::remove_file(source)?;
        return Ok(false);
    }
    fs::copy(&source, target)?;
    fs::File::open(source)?.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))?;
    Ok(true)
}
pub fn save(root: &Path, key: &str, source: &Path) -> Result<()> {
    ensure!(valid_key(key), "Invalid package cache key");
    let size = fs::metadata(source)?.len();
    ensure!(size <= ARCHIVE_LIMIT, "Package cache archive exceeds limit");
    let _lock = lock(root)?;
    let dir = root.join("packages");
    crate::util::private_dir(&dir)?;
    let target = dir.join(format!("{key}.tar"));
    // Remove the replaced entry first; cache insertion failure may lose a cache hit, never evidence.
    if target.exists() {
        fs::remove_file(&target)?;
    }
    prune_locked(root, CACHE_LIMIT - size, SystemTime::now())?;
    let temporary = tempfile::Builder::new()
        .prefix(".crow-package-")
        .tempfile_in(dir)?;
    fs::copy(source, temporary.path())?;
    temporary.persist(target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_expires_without_new_preparations_and_discards_legacy_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"archive").unwrap();
        let key = "a".repeat(64);
        save(root.path(), &key, &source).unwrap();
        let archive = root.path().join("packages").join(format!("{key}.tar"));
        let old = SystemTime::now() - MAX_AGE - Duration::from_secs(1);
        fs::File::open(&archive)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let legacy = root.path().join(format!("{}.tar", "b".repeat(64)));
        fs::write(&legacy, b"old workspace").unwrap();
        assert!(maintain(root.path()).unwrap() > 0);
        assert!(!archive.exists() && !legacy.exists());
        assert!(source.exists());
    }
    #[test]
    fn reads_touch_access_time_and_pruning_removes_oldest_entries() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"1234").unwrap();
        for key in ["a".repeat(64), "b".repeat(64)] {
            save(root.path(), &key, &source).unwrap();
        }
        let a = root
            .path()
            .join("packages")
            .join(format!("{}.tar", "a".repeat(64)));
        fs::File::open(&a)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(prune_locked(root.path(), 4, SystemTime::now()).unwrap(), 4);
        assert!(!a.exists());
        assert!(load(root.path(), &"b".repeat(64), &root.path().join("copy")).unwrap());
        assert_eq!(fs::read(root.path().join("copy")).unwrap(), b"1234");
    }

    fn git(dir: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet"]);
        git(
            root.path(),
            &["config", "user.email", "cache-test@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "Cache test"]);
        root
    }
    fn commit(dir: &Path) -> String {
        git(dir, &["add", "."]);
        git(dir, &["commit", "--quiet", "-m", "fixture"]);
        git(dir, &["rev-parse", "HEAD"])
    }
    fn source(dir: &Path, revision: &str) -> Value {
        json!({"dir":dir,"head":revision,"base":revision})
    }

    #[tokio::test]
    async fn plans_reuse_dependencies_across_code_changes_but_invalidate_changed_locks() {
        let repo = repository();
        fs::write(repo.path().join("package.json"), r#"{"name":"example"}"#).unwrap();
        fs::write(
            repo.path().join("package-lock.json"),
            r#"{"lockfileVersion":3}"#,
        )
        .unwrap();
        fs::write(repo.path().join("app.js"), "first source").unwrap();
        let first = commit(repo.path());
        let original_source = source(repo.path(), &first);
        let original = plan(&original_source, "head", "owner/repo", "pr:1", "image")
            .await
            .unwrap()
            .unwrap();
        fs::write(repo.path().join("app.js"), "fixed source").unwrap();
        let fixed = commit(repo.path());
        let same_dependencies = plan(
            &source(repo.path(), &fixed),
            "head",
            "owner/repo",
            "pr:1",
            "image",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(original.key, same_dependencies.key);
        // Working-directory edits and a different checkout location cannot change
        // the pinned dependency plan or prevent reuse.
        fs::write(
            repo.path().join("package-lock.json"),
            "uncommitted malicious edit",
        )
        .unwrap();
        let pinned = plan(&original_source, "head", "owner/repo", "pr:1", "image")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.key, pinned.key);
        let other_checkout = tempfile::tempdir().unwrap();
        git(
            other_checkout.path(),
            &["clone", "--quiet", repo.path().to_str().unwrap(), "copy"],
        );
        let other = plan(
            &source(&other_checkout.path().join("copy"), &first),
            "head",
            "owner/repo",
            "pr:1",
            "image",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(original.key, other.key);
        let changed_lock = commit(repo.path());
        let changed = plan(
            &source(repo.path(), &changed_lock),
            "head",
            "owner/repo",
            "pr:1",
            "image",
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(original.key, changed.key);
    }

    #[tokio::test]
    async fn cache_namespaces_separate_repositories_prs_images_and_base_from_head() {
        let repo = repository();
        fs::write(repo.path().join("package-lock.json"), "{}").unwrap();
        let commit = commit(repo.path());
        let source = source(repo.path(), &commit);
        let original = plan(&source, "head", "owner/repo", "pr:1", "image")
            .await
            .unwrap()
            .unwrap();
        for (revision, repository, scope, image) in [
            ("base", "owner/repo", "pr:1", "image"),
            ("head", "other/repo", "pr:1", "image"),
            ("head", "owner/repo", "pr:2", "image"),
            ("head", "owner/repo", "pr:1", "other-image"),
        ] {
            let separate = plan(&source, revision, repository, scope, image)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(original.key, separate.key);
        }
    }

    #[tokio::test]
    async fn plans_derive_cargo_and_go_integrity_from_pinned_git_blobs() {
        let repo = repository();
        let checksum = "AB".repeat(32);
        fs::write(
            repo.path().join("Cargo.lock"),
            format!(
                r#"
version = 3
[[package]]
name = "safe-crate"
version = "1.2.3"
checksum = "{checksum}"
[[package]]
name = "../escape"
version = "1.0.0"
checksum = "{checksum}"
[[package]]
name = "no-checksum"
version = "1.0.0"
"#
            ),
        )
        .unwrap();
        fs::write(
            repo.path().join("go.sum"),
            concat!(
                "github.com/Example/Module v1.2.3 h1:zipchecksum\n",
                "github.com/Example/Module v1.2.3/go.mod h1:modchecksum\n",
                "../escape v1.0.0 h1:invalid\n",
                "example.org/unsigned v1.0.0 nope\n",
            ),
        )
        .unwrap();
        let commit = commit(repo.path());
        let plan = plan(
            &source(repo.path(), &commit),
            "head",
            "owner/repo",
            "pr:1",
            "image",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.pins["cargo"],
            json!({"safe-crate-1.2.3.crate":[checksum.to_lowercase()]})
        );
        assert_eq!(
            plan.pins["go"],
            json!({
                "github.com/!example/!module/@v/v1.2.3.zip":"h1:zipchecksum",
                "github.com/!example/!module/@v/v1.2.3.mod":"h1:modchecksum"
            })
        );
        assert_eq!(plan.pins["npm"], false);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tracked_cache_home_symlinks_and_gitlinks_disable_package_caching() {
        let repo = repository();
        fs::write(repo.path().join("package-lock.json"), "{}").unwrap();
        let original = commit(repo.path());
        assert!(
            plan(
                &source(repo.path(), &original),
                "head",
                "owner/repo",
                "pr:1",
                "image"
            )
            .await
            .unwrap()
            .is_some()
        );

        // A source-only PR update retains the lockfile cache key. It must not
        // import a directory where pinned source now requires a symlink.
        std::os::unix::fs::symlink("home", repo.path().join(".crow-home")).unwrap();
        let linked = commit(repo.path());
        assert!(
            plan(
                &source(repo.path(), &linked),
                "head",
                "owner/repo",
                "pr:1",
                "image"
            )
            .await
            .unwrap()
            .is_none()
        );

        fs::remove_file(repo.path().join(".crow-home")).unwrap();
        git(repo.path(), &["rm", "--cached", ".crow-home"]);
        git(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &original,
                ".crow-home",
            ],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "cache home submodule"],
        );
        let submodule = git(repo.path(), &["rev-parse", "HEAD"]);
        assert!(
            plan(
                &source(repo.path(), &submodule),
                "head",
                "owner/repo",
                "pr:1",
                "image"
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_locks_and_unpinned_manifests_do_not_enable_package_caching() {
        let repo = repository();
        fs::write(repo.path().join("real-lock.json"), "{}").unwrap();
        fs::write(repo.path().join("package.json"), "{}").unwrap();
        std::os::unix::fs::symlink("real-lock.json", repo.path().join("package-lock.json"))
            .unwrap();
        let commit = commit(repo.path());
        assert!(
            plan(
                &source(repo.path(), &commit),
                "head",
                "owner/repo",
                "pr:1",
                "image"
            )
            .await
            .unwrap()
            .is_none()
        );
    }
}
