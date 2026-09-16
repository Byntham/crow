//! Native release installation. Updates stage immutable binaries before changing
//! `current`, so the caller can hold its drain and readiness checks around activation.
use anyhow::{Context, Result, bail, ensure};
use flate2::read::MultiGzDecoder;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const ORIGIN: &str = "https://downloads.birdapp.dev";
const CODEX_RELEASE: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_EXTRACTED: u64 = 512 * 1024 * 1024;
const MAX_CODEX_ARCHIVE: u64 = 512 * 1024 * 1024;
const MAX_CODEX_EXTRACTED: u64 = 2 * 1024 * 1024 * 1024;
const CODEX_FILES: &[&str] = &[
    "bin/codex",
    "bin/codex-code-mode-host",
    "codex-path/rg",
    "codex-resources/bwrap",
    "codex-package.json",
];
const CODEX_OPTIONAL: &[&str] = &["codex-resources/zsh/bin/zsh"];
const CODEX_DIRS: &[&str] = &[
    "bin/",
    "codex-path/",
    "codex-resources/",
    "codex-resources/zsh/",
    "codex-resources/zsh/bin/",
];

fn version_parts(version: &str) -> Result<[u64; 3]> {
    let parts: Vec<_> = version.split('.').collect();
    ensure!(
        parts.len() == 3,
        "Invalid stable Crow release version: {version}"
    );
    let mut parsed = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        ensure!(
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0')),
            "Invalid stable release version: {version}"
        );
        parsed[index] = part.parse().context("Invalid release version")?;
    }
    Ok(parsed)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut output = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    Ok(output)
}
pub fn binary_path(root: &Path) -> PathBuf {
    std::path::absolute(root)
        .unwrap_or_else(|_| root.to_owned())
        .join("current/crow")
}
fn directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "Refusing non-directory or symlink {}",
        path.display()
    );
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
fn checksum(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Uses the shared PID lock and its persistent advisory guard so native
/// installers also respect an updater that still runs the legacy executable.
pub struct InstallLock {
    _lock: crate::util::RuntimeLock,
}
pub fn acquire_install_lock(root: &Path) -> Result<InstallLock> {
    directory(root)?;
    for name in ["install.lock", "install.lock.guard"] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(info) => ensure!(
                info.file_type().is_file(),
                "Installation lock is not a regular file"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(InstallLock {
        _lock: crate::util::acquire_lock(&root.join("install.lock"))?,
    })
}
fn link_target(path: &Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(info) => {
            ensure!(
                info.file_type().is_symlink(),
                "Refusing to replace non-symlink {}",
                path.display()
            );
            Ok(Some(fs::read_link(path)?))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn replace_link(path: &Path, target: &Path) -> Result<()> {
    let parent = path.parent().context("Symlink has no parent")?;
    directory(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".crow-link-")
        .tempdir_in(parent)?;
    let link = temporary.path().join("link");
    symlink(target, &link)?;
    fs::rename(&link, path)?;
    sync_dir(parent)
}
fn stage_binary(root: &Path, source: &Path, version: &str) -> Result<PathBuf> {
    version_parts(version)?;
    let source_info = fs::metadata(source)?;
    ensure!(
        source_info.is_file() && source_info.len() > 0 && source_info.len() <= MAX_EXTRACTED,
        "Crow executable must be a nonempty regular file within the release size limit"
    );
    let releases = absolute(root)?.join("releases");
    directory(&releases)?;
    let expected = checksum(source)?;
    let destination = releases.join(format!("{version}-{}", &expected[..16]));
    let executable = destination.join("crow");
    match fs::symlink_metadata(&destination) {
        Ok(info) => {
            ensure!(
                info.file_type().is_dir()
                    && fs::symlink_metadata(&executable)?.file_type().is_file()
                    && checksum(&executable)? == expected,
                "Existing Crow release has unexpected contents"
            );
            return Ok(executable);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&releases)?;
    let candidate = stage.path().join("crow");
    fs::copy(source, &candidate)?;
    ensure!(
        checksum(&candidate)? == expected,
        "Crow executable changed during installation"
    );
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))?;
    File::open(&candidate)?.sync_all()?;
    sync_dir(stage.path())?;
    fs::rename(stage.path(), &destination)?;
    sync_dir(&releases)?;
    Ok(executable)
}

#[derive(Debug)]
pub struct Activation {
    current: PathBuf,
    installed: PathBuf,
    previous: Option<PathBuf>,
}
impl Activation {
    /// Explicit rollback refuses to undo another process's later activation.
    pub fn rollback(self) -> Result<()> {
        ensure!(
            link_target(&self.current)?.as_ref() == Some(&self.installed),
            "Crow installation changed again; refusing rollback"
        );
        if let Some(previous) = self.previous {
            replace_link(&self.current, &previous)
        } else {
            fs::remove_file(&self.current)?;
            sync_dir(self.current.parent().unwrap())
        }
    }
}
#[derive(Debug)]
pub struct PreparedUpdate {
    pub version: String,
    pub executable: PathBuf,
    root: PathBuf,
}
impl PreparedUpdate {
    pub fn activate(&self) -> Result<Activation> {
        activate(&self.root, &self.executable)
    }
}
fn activate(root: &Path, executable: &Path) -> Result<Activation> {
    let current = absolute(root)?.join("current");
    let previous = link_target(&current)?;
    let installed = executable
        .parent()
        .context("Executable has no parent")?
        .to_owned();
    let activation = Activation {
        current,
        installed,
        previous,
    };
    if let Err(error) = replace_link(&activation.current, &activation.installed) {
        // A directory sync may fail after rename has already succeeded.
        if link_target(&activation.current)?.as_ref() == Some(&activation.installed) {
            activation
                .rollback()
                .context("Activation failed and rollback also failed")?;
        }
        return Err(error);
    }
    Ok(activation)
}
fn bin_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CROW_BIN_DIR").filter(|p| !p.is_empty()) {
        return absolute(Path::new(&path));
    }
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/bin"))
}
fn install_binary(root: &Path, source: &Path, version: &str, bin_dir: &Path) -> Result<PathBuf> {
    install_binary_with(root, source, version, bin_dir, replace_link)
}
fn install_binary_with(
    root: &Path,
    source: &Path,
    version: &str,
    bin_dir: &Path,
    replace_entry: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<PathBuf> {
    let root = absolute(root)?;
    let entry = absolute(bin_dir)?.join("crow");
    let previous_entry = link_target(&entry)?;
    if let Some(target) = &previous_entry {
        ensure!(
            absolute(&entry.parent().unwrap().join(target))? == binary_path(&root),
            "Refusing to replace unrelated executable {}",
            entry.display()
        );
    }
    link_target(&root.join("current"))?;
    let staged = stage_binary(&root, source, version)?;
    let activation = activate(&root, &staged)?;
    let installed_entry = binary_path(&root);
    if let Err(error) = replace_entry(&entry, &installed_entry) {
        // The rename can succeed before the directory fsync fails. Restore the
        // CLI link as well as current, without touching an unrelated replacement.
        let entry_rollback = (|| -> Result<()> {
            match fs::read_link(&entry) {
                Ok(target)
                    if target == installed_entry && previous_entry.as_ref() != Some(&target) =>
                {
                    if let Some(previous) = previous_entry {
                        replace_link(&entry, &previous)?;
                    } else {
                        fs::remove_file(&entry)?;
                        sync_dir(entry.parent().unwrap())?;
                    }
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                    ) => {}
                Err(e) => return Err(e.into()),
            }
            Ok(())
        })();
        // Both reversals must be attempted even if one fails.
        let activation_rollback = activation.rollback();
        if entry_rollback.is_err() || activation_rollback.is_err() {
            bail!(
                "Installation failed: {error:#}. CLI link rollback: {entry_rollback:?}. Current release rollback: {activation_rollback:?}"
            );
        }
        return Err(error);
    }
    Ok(installed_entry)
}
fn install_downloaded_from(
    root: &Path,
    source: &Path,
    version: &str,
    bin_dir: &Path,
) -> Result<PathBuf> {
    let _lock = acquire_install_lock(root)?;
    match fs::symlink_metadata(root.join("current")) {
        Ok(_) => {
            let same = match checksum(&binary_path(root)) {
                Ok(existing) => existing == checksum(source)?,
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    false
                }
                Err(e) => return Err(e),
            };
            ensure!(
                same,
                "A different Crow installation already exists. Use its crow update command to upgrade, or its crow setup command to continue onboarding. The installer has left it unchanged."
            );
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    install_binary(root, source, version, bin_dir)
}
pub async fn install_downloaded(root: &Path) -> Result<PathBuf> {
    install_downloaded_from(
        root,
        &std::env::current_exe()?,
        env!("CARGO_PKG_VERSION"),
        &bin_directory()?,
    )
}

fn client(redirects: bool, seconds: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Crow")
        .redirect(if redirects {
            reqwest::redirect::Policy::limited(5)
        } else {
            reqwest::redirect::Policy::none()
        })
        .timeout(Duration::from_secs(seconds))
        .https_only(true)
        .build()?)
}
async fn download(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    limit: u64,
) -> Result<(u64, String)> {
    let response = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Release download failed ({})",
        response.status()
    );
    if let Some(length) = response.content_length() {
        ensure!(length <= limit, "Release exceeds its size limit");
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let mut stream = response.bytes_stream();
    let mut count = 0u64;
    let mut hash = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        count = count
            .checked_add(chunk.len() as u64)
            .context("Release size overflow")?;
        ensure!(count <= limit, "Release exceeds its size limit");
        hash.update(&chunk);
        output.write_all(&chunk)?;
    }
    output.sync_all()?;
    Ok((count, hex::encode(hash.finalize())))
}
async fn fetch_bytes(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?;
    ensure!(
        response.status().is_success(),
        "Release download failed ({})",
        response.status()
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "Release metadata exceeds its size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn latest(client: &reqwest::Client) -> Result<String> {
    let bytes = fetch_bytes(client, &format!("{ORIGIN}/latest.txt"), 128).await?;
    let metadata = std::str::from_utf8(&bytes)?;
    let version = metadata.strip_suffix('\n').unwrap_or(metadata);
    version_parts(version)?;
    Ok(version.to_owned())
}
pub async fn check_update(current: &str) -> Result<Value> {
    let result: Result<Value> = async {
        let version = latest(&client(false, 120)?).await?;
        Ok(json!({"available": version_parts(&version)? > version_parts(current)?, "version": version}))
    }.await;
    Ok(result.unwrap_or_else(|e| json!({"available": false, "warning": e.to_string()})))
}
fn verify_sums(sums: &[u8], name: &str, digest: &str) -> Result<()> {
    let text = std::str::from_utf8(sums)?;
    let matches: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let bytes = line.as_bytes();
            if bytes.len() > 66
                && bytes[64] == b' '
                && matches!(bytes[65], b' ' | b'*')
                && bytes[..64].iter().all(|b| b.is_ascii_hexdigit())
                && &line[66..] == name
            {
                Some(&line[..64])
            } else {
                None
            }
        })
        .collect();
    ensure!(
        matches.len() == 1 && matches[0].eq_ignore_ascii_case(digest),
        "Crow release checksum verification failed"
    );
    Ok(())
}

// Unlike Read::take, this reader reports oversized data instead of presenting a
// forged EOF to the archive parser. It also bounds gzip expansion of padding.
struct LimitedRead<R> {
    inner: R,
    remaining: u64,
}
impl<R: Read> Read for LimitedRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer
            .len()
            .min(self.remaining.saturating_add(1).min(usize::MAX as u64) as usize);
        let size = self.inner.read(&mut buffer[..limit])?;
        if size as u64 > self.remaining {
            return Err(io::Error::other(
                "Release archive exceeds its extracted size limit",
            ));
        }
        self.remaining -= size as u64;
        Ok(size)
    }
}
fn extract_archive(archive: &Path, output: &Path, codex: bool) -> Result<()> {
    directory(output)?;
    let decoded = MultiGzDecoder::new(File::open(archive)?);
    let mut archive = tar::Archive::new(LimitedRead {
        inner: decoded,
        remaining: if codex {
            MAX_CODEX_EXTRACTED
        } else {
            MAX_EXTRACTED
        },
    });
    let mut seen = HashSet::new();
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let raw = entry.path_bytes();
        let name =
            std::str::from_utf8(raw.as_ref()).context("Release archive path is not UTF-8")?;
        let name = if !codex {
            name.strip_prefix("./").unwrap_or(name)
        } else {
            name
        }
        .to_owned();
        let kind = entry.header().entry_type();
        let regular = if codex {
            CODEX_FILES.contains(&name.as_str()) || CODEX_OPTIONAL.contains(&name.as_str())
        } else {
            [
                "crow",
                "THIRD_PARTY_NOTICES",
                "LICENSE",
                "LICENSE.txt",
                "NODE-LICENSE",
                "NODE-LICENSE.txt",
            ]
            .contains(&name.as_str())
        };
        let dir = codex && CODEX_DIRS.contains(&name.as_str());
        ensure!(
            seen.insert(name.clone()) && ((regular && kind.is_file()) || (dir && kind.is_dir())),
            "Unexpected or unsafe release archive entry: {name}"
        );
        if dir {
            ensure!(entry.size() == 0, "Archive directory contains data");
            continue;
        }
        // Only allowlisted literal names reach the filesystem. Neither tar's
        // permission handling nor its symlink extraction is used.
        let destination = output.join(&name);
        directory(destination.parent().unwrap())?;
        let mode = if name == "codex-package.json" {
            0o600
        } else if codex {
            0o700
        } else {
            0o755
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&destination)?;
        let bytes = io::copy(&mut entry, &mut file)?;
        ensure!(
            bytes > 0 || name != "crow",
            "Crow release archive has no executable"
        );
        file.sync_all()?;
    }
    // Read to the gzip trailer to validate CRCs and reject hidden trailing entries.
    let mut remainder = archive.into_inner();
    let mut buffer = [0u8; 8192];
    loop {
        let count = remainder.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            buffer[..count].iter().all(|b| *b == 0),
            "Release archive contains trailing data"
        );
    }
    if codex {
        for required in CODEX_FILES {
            ensure!(
                seen.contains(*required),
                "Codex package is missing {required}"
            );
        }
    } else {
        ensure!(
            seen.contains("crow"),
            "Crow release archive has no executable"
        );
    }
    Ok(())
}
fn architecture() -> Result<(&'static str, &'static str, u16)> {
    ensure!(
        std::env::consts::OS == "linux",
        "Automatic binary installation supports Linux x64 and arm64"
    );
    match std::env::consts::ARCH {
        "x86_64" => Ok(("x64", "x86_64-unknown-linux-musl", 62)),
        "aarch64" => Ok(("arm64", "aarch64-unknown-linux-musl", 183)),
        _ => bail!("Automatic binary installation supports Linux x64 and arm64"),
    }
}
fn verify_elf(path: &Path, machine: u16) -> Result<()> {
    let mut header = [0u8; 20];
    File::open(path)?
        .read_exact(&mut header)
        .context("Truncated release executable")?;
    ensure!(
        &header[..4] == b"\x7fELF"
            && header[4] == 2
            && header[5] == 1
            && u16::from_le_bytes([header[18], header[19]]) == machine,
        "Downloaded executable does not match the requested Linux architecture"
    );
    Ok(())
}
async fn executable_version(executable: &Path) -> Result<String> {
    let output = crate::process::run(
        executable
            .to_str()
            .context("Executable path is not UTF-8")?,
        &["--version".to_owned()],
        crate::process::RunOptions {
            timeout: Some(Duration::from_secs(30)),
            max_output: 4096,
            ..Default::default()
        },
    )
    .await?;
    Ok(output.stdout.trim().to_owned())
}
pub async fn prepare_update(root: &Path, current: &str) -> Result<Option<PreparedUpdate>> {
    let (arch, _, machine) = architecture()?;
    let client = client(false, 120)?;
    let version = latest(&client).await?;
    if version_parts(&version)? <= version_parts(current)? {
        return Ok(None);
    }
    directory(root)?;
    let stage = tempfile::Builder::new()
        .prefix(".update-")
        .tempdir_in(root)?;
    let name = format!("crow-v{version}-linux-{arch}.tar.gz");
    let archive = stage.path().join(&name);
    let (_, digest) = download(
        &client,
        &format!("{ORIGIN}/releases/v{version}/{name}"),
        &archive,
        MAX_ARCHIVE,
    )
    .await?;
    let sums = fetch_bytes(
        &client,
        &format!("{ORIGIN}/releases/v{version}/SHA256SUMS"),
        1024 * 1024,
    )
    .await?;
    verify_sums(&sums, &name, &digest)?;
    let unpacked = stage.path().join("unpacked");
    extract_archive(&archive, &unpacked, false)?;
    let candidate = unpacked.join("crow");
    verify_elf(&candidate, machine)?;
    let actual = executable_version(&candidate).await?;
    ensure!(
        [
            version.clone(),
            format!("crow {version}"),
            format!("Crow {version}")
        ]
        .contains(&actual),
        "Downloaded Crow executable reported an unexpected version"
    );
    let executable = stage_binary(root, &candidate, &version)?;
    Ok(Some(PreparedUpdate {
        version,
        executable,
        root: absolute(root)?,
    }))
}

fn codex_asset(release: &Value, target: &str) -> Result<(String, String, String, u64)> {
    let tag = release["tag_name"]
        .as_str()
        .context("Invalid official Codex release metadata")?;
    let version = tag
        .strip_prefix("rust-v")
        .context("Invalid official Codex release tag")?;
    version_parts(version)?;
    ensure!(
        release["draft"] != true && release["prerelease"] != true,
        "Invalid official Codex release metadata"
    );
    let name = format!("codex-package-{target}.tar.gz");
    let assets = release["assets"]
        .as_array()
        .context("Invalid official Codex release assets")?;
    let matches: Vec<_> = assets.iter().filter(|v| v["name"] == name).collect();
    ensure!(
        matches.len() == 1,
        "Official Codex package has no unique release asset"
    );
    let asset = matches[0];
    let expected_url = format!("https://github.com/openai/codex/releases/download/{tag}/{name}");
    let digest = asset["digest"].as_str().unwrap_or_default();
    let size = asset["size"].as_u64().unwrap_or_default();
    ensure!(
        asset["browser_download_url"] == expected_url
            && digest.starts_with("sha256:")
            && digest.len() == 71
            && digest.as_bytes()[7..]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            && size > 0
            && size <= MAX_CODEX_ARCHIVE,
        "Official Codex package is missing a valid download URL, size, or published SHA-256 digest"
    );
    Ok((
        version.to_owned(),
        expected_url,
        digest[7..].to_owned(),
        size,
    ))
}
fn verify_codex_metadata(metadata: &Value, version: &str, target: &str) -> Result<()> {
    ensure!(
        metadata["layoutVersion"] == 1
            && metadata["version"] == version
            && metadata["target"] == target
            && metadata["variant"] == "codex"
            && metadata["entrypoint"] == "bin/codex"
            && metadata["resourcesDir"] == "codex-resources"
            && metadata["pathDir"] == "codex-path",
        "Codex package metadata does not match the requested release"
    );
    Ok(())
}
pub async fn install_codex(root: &Path) -> Result<PathBuf> {
    let (_, target, machine) = architecture()?;
    let client = client(true, 300)?;
    let release: Value =
        serde_json::from_slice(&fetch_bytes(&client, CODEX_RELEASE, 4 * 1024 * 1024).await?)?;
    let (version, url, digest, size) = codex_asset(&release, target)?;
    let tools = absolute(root)?.join("tools");
    directory(&tools)?;
    let stage = tempfile::Builder::new()
        .prefix(".codex-install-")
        .tempdir_in(&tools)?;
    let archive = stage.path().join("package.tar.gz");
    let (actual_size, actual_digest) = download(&client, &url, &archive, size).await?;
    ensure!(
        actual_size == size && actual_digest == digest,
        "Codex package SHA-256 checksum or size does not match the official release. Nothing was installed"
    );
    let unpacked = stage.path().join("package");
    extract_archive(&archive, &unpacked, true)?;
    let metadata_file = unpacked.join("codex-package.json");
    ensure!(
        fs::metadata(&metadata_file)?.len() <= 1024 * 1024,
        "Codex package metadata exceeds its size limit"
    );
    let metadata: Value = serde_json::from_slice(&fs::read(metadata_file)?)?;
    verify_codex_metadata(&metadata, &version, target)?;
    let executable = unpacked.join("bin/codex");
    verify_elf(&executable, machine)?;
    ensure!(
        executable_version(&executable).await? == format!("codex-cli {version}"),
        "Installed Codex executable reported an unexpected version"
    );
    let destination = tools.join(format!(
        "codex-{version}-{target}-{:016x}",
        rand::random::<u64>()
    ));
    sync_dir(&unpacked)?;
    fs::rename(&unpacked, &destination)?;
    sync_dir(&tools)?;
    Ok(destination.join("bin/codex"))
}

pub async fn install_command(root: &Path, no_setup: bool) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Run the Crow installer as your normal user, without sudo"
    );
    let executable = install_downloaded(root).await?;
    println!(
        "Installed Crow {} at {}",
        env!("CARGO_PKG_VERSION"),
        executable.display()
    );
    let bin = bin_directory()?;
    if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|p| p == bin) {
        println!(
            "To use crow in this shell, run: export PATH='{}':\"$PATH\"",
            bin.to_string_lossy().replace('\'', "'\"'\"'")
        );
    }
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\"'\"'"));
    let instruction = format!(
        "Start or continue setup with: CROW_HOME={} {} setup",
        quote(&absolute(root)?),
        quote(&executable)
    );
    if no_setup || !io::stdin().is_terminal() {
        println!("{instruction}");
        return Ok(());
    }
    print!("Start Crow setup now? [Y/n]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !["", "y", "yes"].contains(&answer.trim().to_ascii_lowercase().as_str()) {
        println!("{instruction}");
        return Ok(());
    }
    let status = tokio::process::Command::new(executable)
        .arg("setup")
        .env_clear()
        .envs(crate::util::host_env())
        .env("CROW_HOME", absolute(root)?)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    ensure!(status.success(), "Crow setup failed ({status})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn archive(path: &Path, entries: &[(&str, &[u8], u8)]) {
        let writer = GzEncoder::new(File::create(path).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(writer);
        for (name, bytes, kind) in entries {
            let mut header = tar::Header::new_ustar();
            // Populate bytes directly so traversal and link fixtures are not
            // rejected by tar::Builder before reaching our extractor.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(tar::EntryType::new(*kind));
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let bin = temp.path().join("bin");
        let source = temp.path().join("source");
        fs::write(&source, b"original native executable").unwrap();
        (temp, root, bin, source)
    }
    #[test]
    fn strict_versions_and_numeric_order() {
        assert!(version_parts("1.10.0").unwrap() > version_parts("1.9.99").unwrap());
        for bad in [
            "1.2",
            "01.2.3",
            "1.2.3\n",
            "1.2.3-beta",
            "+1.2.3",
            "1.2.3.4",
            "1.2.18446744073709551616",
        ] {
            assert!(version_parts(bad).is_err(), "accepted {bad}");
        }
    }
    #[test]
    fn install_is_durable_and_repeated_bootstrap_preserves_state() {
        let (_temp, root, bin, source) = fixture();
        let path = install_downloaded_from(&root, &source, "0.2.0", &bin).unwrap();
        fs::write(root.join("config.json"), "config").unwrap();
        fs::write(root.join("runtime.db"), "database").unwrap();
        assert_eq!(
            install_downloaded_from(&root, &source, "0.2.0", &bin).unwrap(),
            path
        );
        fs::remove_file(source).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original native executable");
        assert_eq!(fs::read_link(bin.join("crow")).unwrap(), binary_path(&root));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read(root.join("config.json")).unwrap(), b"config");
        assert_eq!(fs::read(root.join("runtime.db")).unwrap(), b"database");
    }
    #[test]
    fn bootstrap_cannot_upgrade_or_repair_broken_current() {
        let (_temp, root, bin, source) = fixture();
        let installed = install_downloaded_from(&root, &source, "0.2.0", &bin).unwrap();
        fs::write(&source, "replacement").unwrap();
        let error = install_downloaded_from(&root, &source, "0.3.0", &bin).unwrap_err();
        assert!(error.to_string().contains("crow update"));
        assert_eq!(fs::read(installed).unwrap(), b"original native executable");
        fs::remove_file(root.join("current")).unwrap();
        symlink("missing", root.join("current")).unwrap();
        assert!(install_downloaded_from(&root, &source, "0.3.0", &bin).is_err());
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            PathBuf::from("missing")
        );
    }
    #[test]
    fn concurrent_installation_and_symlink_lock_are_rejected() {
        let (temp, root, bin, source) = fixture();
        let held = acquire_install_lock(&root).unwrap();
        assert!(
            install_downloaded_from(&root, &source, "0.2.0", &bin)
                .unwrap_err()
                .to_string()
                .contains("lock")
        );
        drop(held);
        install_downloaded_from(&root, &source, "0.2.0", &bin).unwrap();
        assert!(!root.join("install.lock").exists());
        let victim = temp.path().join("victim");
        fs::write(&victim, "preserve").unwrap();
        symlink(&victim, root.join("install.lock")).unwrap();
        assert!(acquire_install_lock(&root).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"preserve");
    }
    #[test]
    fn installation_respects_live_legacy_pid_lock() {
        let (_temp, root, bin, source) = fixture();
        directory(&root).unwrap();
        fs::write(
            root.join("install.lock"),
            json!({"pid": std::process::id(), "nonce": "legacy", "start": null}).to_string(),
        )
        .unwrap();
        assert!(install_downloaded_from(&root, &source, "0.2.0", &bin).is_err());
        assert!(!root.join("current").exists());
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.join("install.lock")).unwrap()).unwrap();
        assert_eq!(persisted["nonce"], "legacy");
    }
    #[test]
    fn unrelated_executable_and_current_directory_are_preserved() {
        let (_temp, root, bin, source) = fixture();
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("crow"), "other program").unwrap();
        assert!(install_downloaded_from(&root, &source, "0.2.0", &bin).is_err());
        assert_eq!(fs::read(bin.join("crow")).unwrap(), b"other program");
        fs::remove_file(bin.join("crow")).unwrap();
        symlink("/unrelated/crow", bin.join("crow")).unwrap();
        assert!(install_downloaded_from(&root, &source, "0.2.0", &bin).is_err());
        fs::remove_file(bin.join("crow")).unwrap();
        fs::create_dir(root.join("current")).unwrap();
        assert!(install_binary(&root, &source, "0.2.0", &bin).is_err());
        assert!(root.join("current").is_dir());
    }
    #[test]
    fn activation_can_rollback_but_never_overwrite_later_activation() {
        let (_temp, root, bin, source) = fixture();
        install_downloaded_from(&root, &source, "0.2.0", &bin).unwrap();
        let original = fs::read_link(root.join("current")).unwrap();
        fs::write(&source, "next binary").unwrap();
        let next = stage_binary(&root, &source, "0.3.0").unwrap();
        activate(&root, &next).unwrap().rollback().unwrap();
        assert_eq!(fs::read_link(root.join("current")).unwrap(), original);
        let activation = activate(&root, &next).unwrap();
        replace_link(&root.join("current"), Path::new("later-release")).unwrap();
        assert!(
            activation
                .rollback()
                .unwrap_err()
                .to_string()
                .contains("changed again")
        );
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            PathBuf::from("later-release")
        );
    }
    #[test]
    fn initial_entry_sync_failure_removes_both_new_links() {
        let (_temp, root, bin, source) = fixture();
        let error = install_binary_with(&root, &source, "0.2.0", &bin, |entry, target| {
            replace_link(entry, target)?;
            bail!("injected entry directory sync failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert!(fs::symlink_metadata(root.join("current")).is_err());
        assert!(fs::symlink_metadata(bin.join("crow")).is_err());
    }
    #[test]
    fn entry_sync_failure_restores_previous_relative_link_and_release() {
        let (_temp, root, bin, source) = fixture();
        install_binary(&root, &source, "0.2.0", &bin).unwrap();
        let previous_release = fs::read_link(root.join("current")).unwrap();
        fs::remove_file(bin.join("crow")).unwrap();
        let relative_entry = Path::new("../root/current/crow");
        symlink(relative_entry, bin.join("crow")).unwrap();
        fs::write(&source, "next release").unwrap();
        assert!(
            install_binary_with(&root, &source, "0.3.0", &bin, |entry, target| {
                replace_link(entry, target)?;
                bail!("injected entry directory sync failure")
            })
            .is_err()
        );
        assert_eq!(fs::read_link(bin.join("crow")).unwrap(), relative_entry);
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            previous_release
        );
    }
    #[test]
    fn entry_sync_failure_preserves_unrelated_concurrent_replacement() {
        for symlink_replacement in [true, false] {
            let (_temp, root, bin, source) = fixture();
            assert!(
                install_binary_with(&root, &source, "0.2.0", &bin, |entry, target| {
                    replace_link(entry, target)?;
                    fs::remove_file(entry)?;
                    if symlink_replacement {
                        symlink("/unrelated/crow", entry)?;
                    } else {
                        fs::write(entry, "unrelated executable")?;
                    }
                    bail!("injected entry directory sync failure")
                })
                .is_err()
            );
            assert!(fs::symlink_metadata(root.join("current")).is_err());
            if symlink_replacement {
                assert_eq!(
                    fs::read_link(bin.join("crow")).unwrap(),
                    Path::new("/unrelated/crow")
                );
            } else {
                assert_eq!(fs::read(bin.join("crow")).unwrap(), b"unrelated executable");
            }
        }
    }
    #[test]
    fn stage_detects_tampering_and_refuses_release_symlinks() {
        let (_temp, root, _bin, source) = fixture();
        let executable = stage_binary(&root, &source, "0.2.0").unwrap();
        fs::write(&executable, "tampered").unwrap();
        assert!(stage_binary(&root, &source, "0.2.0").is_err());
        fs::remove_dir_all(executable.parent().unwrap()).unwrap();
        symlink("/tmp", executable.parent().unwrap()).unwrap();
        assert!(stage_binary(&root, &source, "0.2.0").is_err());
    }
    #[test]
    fn checksum_requires_one_exact_archive_record() {
        let digest = "a".repeat(64);
        verify_sums(
            format!("{digest}  release.tar.gz\n").as_bytes(),
            "release.tar.gz",
            &digest,
        )
        .unwrap();
        verify_sums(
            format!("{digest} *release.tar.gz\r\n").as_bytes(),
            "release.tar.gz",
            &digest,
        )
        .unwrap();
        for sums in [
            format!("{digest}  other.tar.gz\n"),
            format!("{digest}  release.tar.gz\n{digest}  release.tar.gz\n"),
            format!("{}  release.tar.gz\n", "b".repeat(64)),
        ] {
            assert!(verify_sums(sums.as_bytes(), "release.tar.gz", &digest).is_err());
        }
    }
    #[test]
    fn archive_rejects_traversal_links_duplicates_extensions_and_missing_binary() {
        for entries in [
            vec![("../crow", b"x".as_slice(), b'0')],
            vec![("/crow", b"x".as_slice(), b'0')],
            vec![("crow", b"x".as_slice(), b'2')],
            vec![("crow", b"x".as_slice(), b'1')],
            vec![("crow", b"x".as_slice(), b'x')],
            vec![
                ("crow", b"x".as_slice(), b'0'),
                ("crow", b"y".as_slice(), b'0'),
            ],
            vec![("LICENSE", b"license".as_slice(), b'0')],
            vec![("crow", b"".as_slice(), b'0')],
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("archive.gz");
            archive(&path, &entries);
            assert!(
                extract_archive(&path, &tmp.path().join("out"), false).is_err(),
                "accepted {entries:?}"
            );
        }
    }
    #[test]
    fn archive_accepts_known_files_and_validates_gzip_trailer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive.gz");
        archive(
            &path,
            &[
                ("./crow", b"executable", b'0'),
                ("LICENSE", b"license", b'0'),
            ],
        );
        extract_archive(&path, &tmp.path().join("valid"), false).unwrap();
        assert_eq!(
            fs::read(tmp.path().join("valid/crow")).unwrap(),
            b"executable"
        );
        let mut bytes = fs::read(&path).unwrap();
        let index = bytes.len() - 8;
        bytes[index] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(extract_archive(&path, &tmp.path().join("corrupt"), false).is_err());
    }
    #[test]
    fn archive_rejects_hidden_data_after_end_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive.gz");
        archive(&path, &[("crow", b"executable", b'0')]);
        let mut plain = Vec::new();
        MultiGzDecoder::new(File::open(&path).unwrap())
            .read_to_end(&mut plain)
            .unwrap();
        plain.extend_from_slice(b"hidden entry");
        let mut encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
        encoder.write_all(&plain).unwrap();
        encoder.finish().unwrap();
        assert!(
            extract_archive(&path, &tmp.path().join("out"), false)
                .unwrap_err()
                .to_string()
                .contains("trailing data")
        );
    }
    #[test]
    fn decompression_limit_reports_an_error_instead_of_eof() {
        let mut reader = LimitedRead {
            inner: b"123456".as_slice(),
            remaining: 5,
        };
        let mut bytes = Vec::new();
        assert!(reader.read_to_end(&mut bytes).is_err());
    }
    #[test]
    fn codex_archive_requires_helpers_and_extracts_with_private_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive.gz");
        let entries: Vec<_> = CODEX_FILES
            .iter()
            .map(|name| (*name, b"content".as_slice(), b'0'))
            .collect();
        archive(&path, &entries);
        let output = tmp.path().join("valid");
        extract_archive(&path, &output, true).unwrap();
        assert_eq!(
            fs::metadata(output.join("bin/codex"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(output.join("codex-package.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        archive(&path, &entries[..1]);
        assert!(
            extract_archive(&path, &tmp.path().join("missing"), true)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
    }
    #[test]
    fn codex_metadata_rejects_substituted_assets() {
        let target = "x86_64-unknown-linux-musl";
        let name = format!("codex-package-{target}.tar.gz");
        let mut release = json!({"tag_name":"rust-v1.2.3", "draft":false, "prerelease":false, "assets":[{"name":name, "browser_download_url":format!("https://github.com/openai/codex/releases/download/rust-v1.2.3/{name}"), "digest":format!("sha256:{}", "a".repeat(64)), "size":123}]});
        assert_eq!(codex_asset(&release, target).unwrap().0, "1.2.3");
        release["assets"][0]["browser_download_url"] = json!("https://attacker.test/codex");
        assert!(codex_asset(&release, target).is_err());
        release["tag_name"] = json!("rust-v1.2.3/../../evil");
        assert!(codex_asset(&release, target).is_err());
        let metadata = json!({"layoutVersion":1, "version":"1.2.3", "target":target, "variant":"codex", "entrypoint":"bin/codex", "resourcesDir":"codex-resources", "pathDir":"codex-path"});
        verify_codex_metadata(&metadata, "1.2.3", target).unwrap();
        assert!(verify_codex_metadata(&metadata, "1.2.4", target).is_err());
    }
    #[test]
    fn elf_check_rejects_wrong_architecture_and_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("crow");
        let mut header = [0u8; 20];
        header[..6].copy_from_slice(b"\x7fELF\x02\x01");
        header[18..20].copy_from_slice(&62u16.to_le_bytes());
        fs::write(&binary, header).unwrap();
        verify_elf(&binary, 62).unwrap();
        assert!(verify_elf(&binary, 183).is_err());
        fs::write(&binary, "#!/bin/sh").unwrap();
        assert!(verify_elf(&binary, 62).is_err());
    }
    #[tokio::test]
    async fn version_check_bounds_output_and_requires_success() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("check");
        fs::write(&executable, "#!/bin/sh\nprintf 'crow 1.2.3\\n'\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(executable_version(&executable).await.unwrap(), "crow 1.2.3");
        fs::write(&executable, "#!/bin/sh\nprintf 'crow 1.2.3\\n'\nexit 1\n").unwrap();
        assert!(executable_version(&executable).await.is_err());
        fs::write(&executable, "#!/bin/sh\nhead -c 5000 /dev/zero\n").unwrap();
        assert!(executable_version(&executable).await.is_err());
    }
}
