//! Encrypted, portable configuration and SQLite backups compatible with Crow v1.
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag, aead::AeadInPlace};
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};

const HEADER: &[u8] = b"CROWBACKUP\x01";
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const NAMES: [&str; 2] = ["config.json", "service.sqlite"];

// The archive includes credentials. Erase its owned strings along with byte buffers.
struct SecretValue(Value);
impl std::ops::Deref for SecretValue {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}
impl std::ops::DerefMut for SecretValue {
    fn deref_mut(&mut self) -> &mut Value {
        &mut self.0
    }
}
impl Drop for SecretValue {
    fn drop(&mut self) {
        fn wipe(value: &mut Value) {
            match value {
                Value::String(s) => s.zeroize(),
                Value::Array(values) => values.iter_mut().for_each(wipe),
                Value::Object(values) => values.values_mut().for_each(wipe),
                _ => {}
            }
        }
        wipe(&mut self.0);
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let full = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for component in full.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options.open(path)?;
    if let Err(error) = output.write_all(bytes).and_then(|()| output.sync_all()) {
        drop(output);
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}
fn read_limited(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let input = File::open(path)?;
    ensure!(
        input.metadata()?.len() <= MAX_BYTES,
        "Backup exceeds the 128 MiB limit"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    input.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Backup exceeds the 128 MiB limit"
    );
    Ok(bytes)
}
fn key_for(secret: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(!secret.is_empty(), "A backup passphrase is required");
    let mut key = Zeroizing::new([0u8; 32]);
    let params = scrypt::Params::new(15, 8, 1, 32).map_err(|e| anyhow!(e.to_string()))?;
    scrypt::scrypt(secret.as_bytes(), salt, &params, key.as_mut())
        .map_err(|e| anyhow!(e.to_string()))?;
    Ok(key)
}
fn validate_database(path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "Backup database integrity check failed");
    for table in ["records", "receipts", "events"] {
        let found: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        ensure!(found, "Backup database is missing {table}");
    }
    Ok(())
}
fn encrypt(payload: &[u8], secret: &str) -> Result<Vec<u8>> {
    let mut salt = [0; 16];
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = key_for(secret, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|e| anyhow!(e.to_string()))?;
    let mut encrypted = Zeroizing::new(payload.to_vec());
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), HEADER, &mut encrypted)
        .map_err(|_| anyhow!("Cannot encrypt backup"))?;
    let mut result = Vec::with_capacity(HEADER.len() + 44 + encrypted.len());
    result.extend_from_slice(HEADER);
    result.extend_from_slice(&salt);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&tag);
    result.extend_from_slice(&encrypted);
    Ok(result)
}
fn decrypt(bytes: &[u8], secret: &str) -> Result<Zeroizing<Vec<u8>>> {
    let offset = HEADER.len();
    ensure!(
        bytes.len() >= offset + 45 && bytes.starts_with(HEADER),
        "Unsupported Crow backup"
    );
    let key = key_for(secret, &bytes[offset..offset + 16])?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|e| anyhow!(e.to_string()))?;
    let mut plaintext = Zeroizing::new(bytes[offset + 44..].to_vec());
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(&bytes[offset + 16..offset + 28]),
            HEADER,
            &mut plaintext,
            Tag::from_slice(&bytes[offset + 28..offset + 44]),
        )
        .map_err(|_| anyhow!("Cannot decrypt backup: incorrect passphrase or damaged file"))?;
    Ok(plaintext)
}

/// Export configuration and a consistent SQLite snapshot. Provider sessions are excluded.
pub fn export_backup(root: &Path, file: &Path, secret: &str) -> Result<Value> {
    let root = absolute(root)?;
    let file = absolute(file)?;
    ensure!(
        !NAMES.iter().any(|name| file == root.join(name)),
        "Backup output cannot replace live Crow state"
    );
    ensure!(!secret.is_empty(), "A backup passphrase is required");
    let temp = tempfile::Builder::new()
        .prefix(".backup-")
        .tempdir_in(&root)?;
    let config = read_limited(&root.join("config.json"))?;
    let parsed = SecretValue(serde_json::from_slice(&config)?);
    crate::config::validate_config(&parsed)?;
    let mut files = Map::new();
    files.insert(
        "config.json".into(),
        Value::String(STANDARD.encode(&config)),
    );
    if root.join("service.sqlite").try_exists()? {
        let snapshot = temp.path().join("service.sqlite");
        let db = Connection::open_with_flags(
            root.join("service.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute(
            "VACUUM INTO ?1",
            [snapshot.to_str().context("Invalid snapshot path")?],
        )?;
        drop(db);
        validate_database(&snapshot)?;
        files.insert(
            "service.sqlite".into(),
            Value::String(STANDARD.encode(read_limited(&snapshot)?)),
        );
    }
    let names: Vec<_> = files.keys().cloned().collect();
    let archive = SecretValue(
        json!({"version":1, "createdAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "files":files}),
    );
    let payload = Zeroizing::new(serde_json::to_vec(&archive.0)?);
    ensure!(
        payload.len() as u64 <= MAX_BYTES - 1024,
        "Backup exceeds the 128 MiB limit"
    );
    let encrypted = encrypt(&payload, secret)?;
    private_dir(
        file.parent()
            .context("Backup output needs a parent directory")?,
    )?;
    write_private(&file, &encrypted)?;
    Ok(json!({"file":file,"files":names}))
}

/// Restore requires a stopped runtime. Original files are restored if replacement fails.
pub fn restore_backup(root: &Path, file: &Path, secret: &str) -> Result<Value> {
    let root = absolute(root)?;
    let bytes = read_limited(file)?;
    let plaintext = decrypt(&bytes, secret)?;
    let archive = SecretValue(
        serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow!("Cannot decrypt backup: incorrect passphrase or damaged file"))?,
    );
    let files = archive
        .get("files")
        .and_then(Value::as_object)
        .context("Invalid Crow backup contents")?;
    ensure!(
        archive.get("version") == Some(&json!(1))
            && files.contains_key("config.json")
            && files.keys().all(|name| NAMES.contains(&name.as_str())),
        "Invalid Crow backup contents"
    );
    let mut content: Vec<(String, Zeroizing<Vec<u8>>)> = Vec::new();
    for (name, value) in files {
        let value = value.as_str().context("Invalid backup file encoding")?;
        let decoded = STANDARD
            .decode(value)
            .context("Invalid backup file encoding")?;
        content.push((name.clone(), Zeroizing::new(decoded)));
    }
    let config_bytes = &mut content
        .iter_mut()
        .find(|(name, _)| name == "config.json")
        .unwrap()
        .1;
    let mut config = SecretValue(serde_json::from_slice(config_bytes)?);
    crate::config::validate_config(&config)?;
    config["worker"]["codexHome"] = json!(root.join("codex"));
    *config_bytes = Zeroizing::new(serde_json::to_vec_pretty(&config.0)?);
    config_bytes.push(b'\n');
    private_dir(&root)?;
    let _lock = crate::util::acquire_lock(&root.join("runtime.lock"))?;
    let temp = tempfile::Builder::new()
        .prefix(".restore-")
        .tempdir_in(&root)?;
    for (name, bytes) in &content {
        write_private(&temp.path().join(name), bytes)?;
    }
    if files.contains_key("service.sqlite") {
        validate_database(&temp.path().join("service.sqlite"))?;
    }
    let mut pending = json!({"version":1,"restoredAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),"providerSessionsRestored":false});
    if let Some(created) = archive.get("createdAt") {
        pending["backupCreatedAt"] = created.clone();
    }
    let mut pending_bytes = serde_json::to_vec(&pending)?;
    pending_bytes.push(b'\n');
    write_private(&temp.path().join("restore-pending.json"), &pending_bytes)?;
    let names: Vec<String> = content.iter().map(|(name, _)| name.clone()).collect();
    let mut install = names.clone();
    install.push("restore-pending.json".into());
    if let Err((error, preserve)) = replace_files(&root, temp.path(), &install) {
        if preserve {
            let retained = temp.keep();
            return Err(error.context(format!(
                "Restore failed; previous files were retained in {}",
                retained.display()
            )));
        }
        return Err(error);
    }
    Ok(json!({"root":root,"files":names,"requiresProviderLogin":true}))
}

// Keep rollback recovery files if an external filesystem failure prevents rollback.
fn replace_files(
    root: &Path,
    temp: &Path,
    install: &[String],
) -> std::result::Result<(), (anyhow::Error, bool)> {
    let mut moved: Vec<&str> = Vec::new();
    let mut installed: Vec<&str> = Vec::new();
    let replacement = (|| -> Result<()> {
        for name in [
            "config.json",
            "service.sqlite",
            "service.sqlite-wal",
            "service.sqlite-shm",
            "restore-pending.json",
        ] {
            match fs::symlink_metadata(root.join(name)) {
                Ok(_) => {
                    fs::rename(root.join(name), temp.join(format!("{name}.previous")))?;
                    moved.push(name);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for name in install {
            fs::rename(temp.join(name), root.join(name))?;
            installed.push(name);
        }
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = replacement {
        let rollback = (|| -> Result<()> {
            for name in installed.iter().rev() {
                fs::remove_file(root.join(name))?;
            }
            for name in moved.iter().rev() {
                fs::rename(temp.join(format!("{name}.previous")), root.join(name))?;
            }
            Ok(())
        })();
        return match rollback {
            Ok(()) => Err((error, false)),
            Err(rollback) => Err((anyhow!("{error}; rollback failed: {rollback}"), true)),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const SECRET: &str = "a test-only backup passphrase";
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Value) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir(&root).unwrap();
        let config = json!({
            "version":1,"role":"both","operator":"test-user","publicUrl":null,
            "port":8787,"bind":"127.0.0.1","adminToken":"a".repeat(64),
            "serviceUrl":"http://127.0.0.1:8787",
            "worker":{"id":"test-worker","token":"b".repeat(64),"concurrency":3,
                "codex":"codex","codexHome":root.join("codex"),"model":null,"effort":null,
                "subagents":{"mode":"inherit","max":8},"retry":{"mode":"fixed","count":10,"delayMs":5000},"timeoutMs":0},
            "catchUp":{"enabled":true,"threshold":10},"auditIntervalMs":3600000,"retentionDays":7,
            "ingress":{"type":"funnel"},"app":{"id":123,"slug":"crow-test","pem":"secret app key","webhookSecret":"secret webhook key"},
            "futureField":{"preserve":true}
        });
        fs::write(
            root.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let backup = temp.path().join("backup.crow");
        (temp, root, backup, config)
    }
    fn crafted(file: &Path, files: Value) {
        let bytes = serde_json::to_vec(&json!({"version":1,"files":files})).unwrap();
        fs::write(file, encrypt(&bytes, SECRET).unwrap()).unwrap();
    }
    #[test]
    fn decrypts_node_crypto_legacy_format() {
        // Created by Node crypto.scryptSync/createCipheriv, not this implementation.
        let bytes = STANDARD.decode("Q1JPV0JBQ0tVUAEBAQEBAQEBAQEBAQEBAQEBAgICAgICAgICAgICCCT1XTU4Z2UB32nOer+qjnMAOgiz0aUWYeE4hI2fEZ19tEgGS+g/yg==").unwrap();
        assert_eq!(
            decrypt(&bytes, "legacy passphrase").unwrap().as_slice(),
            br#"{"version":1,"files":{}}"#
        );
    }
    #[test]
    fn wal_snapshot_restores_credentials_and_excludes_provider_home() {
        let (temp, source, file, _) = fixture();
        let db = Connection::open(source.join("service.sqlite")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records (key TEXT, value TEXT); CREATE TABLE receipts(id TEXT); CREATE TABLE events(id TEXT); INSERT INTO records VALUES ('owner/repo','state');").unwrap();
        fs::create_dir(source.join("codex")).unwrap();
        fs::write(source.join("codex/auth.json"), "provider-secret").unwrap();
        let exported = export_backup(&source, &file, SECRET).unwrap();
        assert_eq!(exported["files"], json!(["config.json", "service.sqlite"]));
        assert!(
            !fs::read(&file)
                .unwrap()
                .windows(14)
                .any(|w| w == b"secret app key")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let destination = temp.path().join("restored");
        restore_backup(&destination, &file, SECRET).unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(destination.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["app"]["pem"], "secret app key");
        assert_eq!(
            config["worker"]["codexHome"],
            json!(destination.join("codex"))
        );
        assert_eq!(config["futureField"]["preserve"], true);
        let restored = Connection::open(destination.join("service.sqlite")).unwrap();
        let state: String = restored
            .query_row(
                "SELECT value FROM records WHERE key='owner/repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "state");
        let pending: Value =
            serde_json::from_slice(&fs::read(destination.join("restore-pending.json")).unwrap())
                .unwrap();
        assert_eq!(pending["providerSessionsRestored"], false);
        assert!(!destination.join("codex/auth.json").exists());
    }
    #[test]
    fn wrong_secret_and_tampering_preserve_live_state() {
        let (_temp, source, file, _) = fixture();
        export_backup(&source, &file, SECRET).unwrap();
        let original = fs::read(source.join("config.json")).unwrap();
        assert!(
            restore_backup(&source, &file, "wrong")
                .unwrap_err()
                .to_string()
                .contains("Cannot decrypt")
        );
        let mut bytes = fs::read(&file).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&file, bytes).unwrap();
        assert!(
            restore_backup(&source, &file, SECRET)
                .unwrap_err()
                .to_string()
                .contains("Cannot decrypt")
        );
        assert_eq!(fs::read(source.join("config.json")).unwrap(), original);
    }
    #[test]
    fn refuses_overwrite_and_active_runtime() {
        let (_temp, source, file, _) = fixture();
        export_backup(&source, &file, SECRET).unwrap();
        let original = fs::read(&file).unwrap();
        assert!(export_backup(&source, &file, SECRET).is_err());
        assert_eq!(fs::read(&file).unwrap(), original);
        let _lock = crate::util::acquire_lock(&source.join("runtime.lock")).unwrap();
        assert!(restore_backup(&source, &file, SECRET).is_err());
        assert!(export_backup(&source, &source.join("config.json"), SECRET).is_err());
    }
    #[test]
    fn rejects_unexpected_names_and_invalid_config_before_replacement() {
        let (_temp, source, file, config) = fixture();
        let original = fs::read(source.join("config.json")).unwrap();
        crafted(
            &file,
            json!({"config.json":STANDARD.encode(&original),"../escaped":"eA=="}),
        );
        assert!(
            restore_backup(&source, &file, SECRET)
                .unwrap_err()
                .to_string()
                .contains("Invalid Crow backup contents")
        );
        let mut invalid = config;
        invalid["version"] = json!(999);
        crafted(
            &file,
            json!({"config.json":STANDARD.encode(serde_json::to_vec(&invalid).unwrap())}),
        );
        assert!(restore_backup(&source, &file, SECRET).is_err());
        assert_eq!(fs::read(source.join("config.json")).unwrap(), original);
    }
    #[test]
    fn rejects_invalid_and_unrelated_sqlite_before_replacement() {
        let (_temp, source, file, config) = fixture();
        let encoded = STANDARD.encode(serde_json::to_vec(&config).unwrap());
        let original = fs::read(source.join("config.json")).unwrap();
        crafted(
            &file,
            json!({"config.json":encoded,"service.sqlite":STANDARD.encode(b"not a database")}),
        );
        assert!(restore_backup(&source, &file, SECRET).is_err());
        let unrelated = source.join("other.sqlite");
        Connection::open(&unrelated)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated(x);")
            .unwrap();
        crafted(
            &file,
            json!({"config.json":encoded,"service.sqlite":STANDARD.encode(fs::read(&unrelated).unwrap())}),
        );
        assert!(
            restore_backup(&source, &file, SECRET)
                .unwrap_err()
                .to_string()
                .contains("missing records")
        );
        assert_eq!(fs::read(source.join("config.json")).unwrap(), original);
    }
    #[test]
    fn replacement_failure_rolls_back_all_state_and_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let stage = temp.path().join("stage");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&stage).unwrap();
        for name in [
            "config.json",
            "service.sqlite",
            "service.sqlite-wal",
            "service.sqlite-shm",
            "restore-pending.json",
        ] {
            fs::write(root.join(name), name).unwrap();
        }
        fs::write(stage.join("config.json"), "replacement").unwrap();
        let (_error, preserve) =
            replace_files(&root, &stage, &["config.json".into(), "missing".into()]).unwrap_err();
        assert!(!preserve);
        for name in [
            "config.json",
            "service.sqlite",
            "service.sqlite-wal",
            "service.sqlite-shm",
            "restore-pending.json",
        ] {
            assert_eq!(fs::read_to_string(root.join(name)).unwrap(), name);
        }
    }
    #[test]
    fn rejects_oversize_without_allocating_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("huge");
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(
            read_limited(&path)
                .unwrap_err()
                .to_string()
                .contains("128 MiB")
        );
    }
    #[test]
    fn config_only_restore_removes_obsolete_database_and_sidecars() {
        let (temp, source, file, _) = fixture();
        export_backup(&source, &file, SECRET).unwrap();
        let root = temp.path().join("restored");
        fs::create_dir(&root).unwrap();
        for name in ["service.sqlite", "service.sqlite-wal", "service.sqlite-shm"] {
            fs::write(root.join(name), "old state").unwrap();
        }
        restore_backup(&root, &file, SECRET).unwrap();
        for name in ["service.sqlite", "service.sqlite-wal", "service.sqlite-shm"] {
            assert!(!root.join(name).exists());
        }
    }
}
