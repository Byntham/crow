use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

pub fn id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
pub fn hash(value: &Value) -> String {
    match value.as_str() {
        Some(s) => hash_bytes(s.as_bytes()),
        None => hash_bytes(value.to_string().as_bytes()),
    }
}
pub fn hash_bytes(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}
pub fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
pub fn repo_name(value: &str) -> Result<String> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
    {
        bail!("Expected owner/repository");
    }
    Ok(value.to_ascii_lowercase())
}
pub fn https_url(value: &str) -> Result<String> {
    let url =
        url::Url::parse(value).context("Expected an HTTPS origin without a path or credentials")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Expected an HTTPS origin without a path or credentials");
    }
    Ok(url.origin().ascii_serialization())
}
pub fn integer(value: &Value, min: i64, max: i64, name: &str) -> Result<i64> {
    // JSON permits 1.0, and JavaScript treated that as an integer.
    let number = value
        .as_f64()
        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= min as f64 && *n <= max as f64)
        .with_context(|| format!("{name} must be an integer between {min} and {max}"))?;
    Ok(number as i64)
}
pub fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
pub fn atomic(path: &Path, value: &Value) -> Result<()> {
    match value.as_str() {
        Some(s) => atomic_bytes(path, s.as_bytes()),
        None => {
            let mut bytes = serde_json::to_vec_pretty(value)?;
            bytes.push(b'\n');
            atomic_bytes(path, &bytes)
        }
    }
}
pub fn atomic_bytes(path: &Path, value: &[u8]) -> Result<()> {
    let directory = parent(path);
    private_dir(directory)?;
    let mut file = tempfile::Builder::new()
        .prefix(".crow-")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    file.write_all(value)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
pub fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("Invalid JSON in {}", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn clean_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "SSL_CERT_FILE",
        "CODEX_CA_CERTIFICATE",
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            env.insert(key.to_owned(), value);
        }
    }
    for (k, v) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("NO_COLOR", "1"),
    ] {
        env.insert(k.into(), v.into());
    }
    env
}
pub fn host_env() -> HashMap<String, String> {
    let mut env = clean_env();
    for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            env.insert(key.to_owned(), value);
        }
    }
    env
}
fn process_start(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 2..)?
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}
fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            return false;
        }
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
fn read_lock(path: &Path) -> Result<Option<Value>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        bail!("Runtime lock must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("Invalid runtime lock size");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
/// Holds an advisory guard until the legacy-compatible PID lock is removed.
/// The guard inode is never unlinked, avoiding races between stale-lock claimants.
pub struct RuntimeLock {
    path: PathBuf,
    nonce: String,
    guard: File,
}
impl Drop for RuntimeLock {
    fn drop(&mut self) {
        if read_lock(&self.path)
            .ok()
            .flatten()
            .and_then(|v| v["nonce"].as_str().map(str::to_owned))
            .as_deref()
            == Some(&self.nonce)
        {
            let _ = fs::remove_file(&self.path);
        }
        let _ = FileExt::unlock(&self.guard);
    }
}
pub fn acquire_lock(path: &Path) -> Result<RuntimeLock> {
    private_dir(parent(path))?;
    let message =
        "Crow is running, or its runtime lock needs inspection. Stop Crow before continuing.";
    let mut guard_path = path.as_os_str().to_os_string();
    guard_path.push(".guard");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let guard = options.open(Path::new(&guard_path))?;
    if !guard.metadata()?.is_file() {
        bail!("Runtime guard must be a regular file");
    }
    guard.try_lock_exclusive().context(message)?;
    if let Some(old) = read_lock(path).context(message)? {
        let pid = old["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context(message)? as u32;
        if process_exists(pid) {
            let start = process_start(pid);
            // Missing process metadata is not evidence that a live owner is stale.
            if start.is_none()
                || old["start"].is_null()
                || old["start"].as_str() == start.as_deref()
            {
                bail!("{message}");
            }
        }
        fs::remove_file(path)?;
    }
    let nonce = id();
    let pid = std::process::id();
    atomic(
        path,
        &json!({"pid":pid,"start":process_start(pid),"nonce":nonce}),
    )?;
    Ok(RuntimeLock {
        path: path.to_owned(),
        nonce,
        guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashes_and_names() {
        assert_eq!(
            hash(&json!("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(repo_name("Owner/Repo.git").unwrap(), "owner/repo.git");
        for name in [
            "../repo", "owner/.", "a/b/c", "a\\b/c", "å/b", "a/", "a/b\n",
        ] {
            assert!(repo_name(name).is_err(), "{name}");
        }
        assert!(equal("é", "é"));
        assert!(!equal("é", "ee"));
    }
    #[test]
    fn origins_reject_credentials_and_paths() {
        assert_eq!(
            https_url("https://EXAMPLE.com:443/").unwrap(),
            "https://example.com"
        );
        for url in [
            "http://example.com",
            "https://a:b@example.com",
            "https://example.com/x",
            "https://example.com/?q=a",
            "https://example.com/#a",
        ] {
            assert!(https_url(url).is_err());
        }
    }
    #[test]
    #[cfg(unix)]
    fn runtime_locks_reject_symlinks_and_malformed_owners() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.lock");
        let target = dir.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();
        assert!(acquire_lock(&path).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(&path).unwrap();
        let guard = dir.path().join("runtime.lock.guard");
        fs::remove_file(&guard).unwrap();
        symlink(&target, &guard).unwrap();
        assert!(acquire_lock(&path).is_err());
        fs::remove_file(&guard).unwrap();
        atomic(&path, &json!({"pid":null})).unwrap();
        assert!(acquire_lock(&path).is_err());
        assert!(path.exists());
    }
    #[test]
    fn atomic_files_and_lock_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/x.json");
        assert!(read_json(&path).unwrap().is_none());
        atomic(&path, &json!({"secret":1})).unwrap();
        assert_eq!(read_json(&path).unwrap().unwrap()["secret"], 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let lock_path = dir.path().join("runtime.lock");
        let lock = acquire_lock(&lock_path).unwrap();
        assert!(acquire_lock(&lock_path).is_err());
        drop(lock);
        assert!(!lock_path.exists());
        atomic(
            &lock_path,
            &json!({"pid":u32::MAX,"start":null,"nonce":"old"}),
        )
        .unwrap();
        let lock = acquire_lock(&lock_path).unwrap();
        atomic(&lock_path, &json!({"nonce":"replacement"})).unwrap();
        drop(lock);
        assert!(lock_path.exists());
    }
}
