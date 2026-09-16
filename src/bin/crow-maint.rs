//! Release administration. Downloaded release executables are never run here.
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const MAX_ARCHIVE: usize = 160 * 1024 * 1024;
const MAX_UNPACKED: usize = 256 * 1024 * 1024;
const DOWNLOADS: &str = "https://downloads.birdapp.dev/";
const ARCHES: [&str; 2] = ["x64", "arm64"];

#[derive(Parser)]
#[command(about = "Crow release and website maintenance")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Package a locally built executable; smoke-test only in the build job.
    Pack {
        #[arg(long, default_value = "target/release/crow")]
        executable: PathBuf,
        #[arg(long, default_value = "dist-release")]
        out: PathBuf,
    },
    VerifyRun,
    Publish {
        directory: PathBuf,
    },
    PrepareSite,
    StageSite {
        #[arg(default_value = "website")]
        directory: PathBuf,
    },
}
#[derive(Clone)]
struct Request {
    version: String,
    commit: String,
    run_id: String,
}
fn stable(v: &str) -> bool {
    let p: Vec<_> = v.split('.').collect();
    p.len() == 3
        && p.iter().all(|s| {
            !s.is_empty()
                && s.bytes().all(|c| c.is_ascii_digit())
                && (s.len() == 1 || !s.starts_with('0'))
        })
}
fn full_commit(v: &str) -> bool {
    v.len() == 40
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn request(version: String, commit: String, run_id: String, confirmation: &str) -> Result<Request> {
    ensure!(stable(&version), "Use a stable VERSION such as 0.2.0.");
    ensure!(
        full_commit(&commit),
        "Provide the full 40-character build commit SHA."
    );
    ensure!(
        !run_id.is_empty()
            && !run_id.starts_with('0')
            && run_id.bytes().all(|c| c.is_ascii_digit()),
        "Provide a numeric build run ID."
    );
    ensure!(
        confirmation == format!("publish {version}"),
        "Confirmation must be exactly: publish {version}"
    );
    Ok(Request {
        version,
        commit,
        run_id,
    })
}
fn release_request() -> Result<Request> {
    request(
        env::var("RELEASE_VERSION")?,
        env::var("BUILD_COMMIT")?,
        env::var("BUILD_RUN_ID")?,
        &env::var("PUBLICATION_CONFIRMATION")?,
    )
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn compare_versions(a: &str, b: &str) -> Ordering {
    a.split('.')
        .zip(b.split('.'))
        .map(|(a, b)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
        .find(|c| *c != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}
fn validate_run(run: &Value, r: &Request, repository: &str) -> Result<()> {
    ensure!(
        run["id"].as_u64().map(|n| n.to_string()).as_deref() == Some(&r.run_id)
            && run["repository"]["full_name"] == repository
            && run["head_repository"]["full_name"] == repository
            && run["head_sha"] == r.commit
            && run["path"] == ".github/workflows/release.yml"
            && run["status"] == "completed"
            && run["conclusion"] == "success"
            && matches!(run["event"].as_str(), Some("push" | "workflow_dispatch")),
        "The selected run must be a successful Crow binary build from this repository at the exact requested commit."
    );
    Ok(())
}
fn field(header: &[u8], start: usize, len: usize) -> Result<&str> {
    let bytes = &header[start..start + len];
    Ok(std::str::from_utf8(
        bytes.split(|b| *b == 0).next().unwrap_or_default(),
    )?)
}
fn octal(header: &[u8], start: usize, len: usize) -> Result<usize> {
    let value = field(header, start, len)?.trim();
    ensure!(
        !value.is_empty() && value.bytes().all(|c| (b'0'..=b'7').contains(&c)),
        "Invalid tar numeric field."
    );
    Ok(usize::from_str_radix(value, 8)?)
}
fn validate_archive(bytes: &[u8], architecture: &str) -> Result<()> {
    ensure!(ARCHES.contains(&architecture), "Unknown architecture.");
    ensure!(
        bytes.len() <= MAX_ARCHIVE,
        "Release archive exceeds the size limit."
    );
    let mut unpacked = Vec::new();
    MultiGzDecoder::new(bytes)
        .take((MAX_UNPACKED + 1) as u64)
        .read_to_end(&mut unpacked)?;
    ensure!(
        unpacked.len() <= MAX_UNPACKED,
        "Release expands beyond the size limit."
    );
    let mut names = BTreeSet::new();
    let mut offset = 0usize;
    while offset + 512 <= unpacked.len() {
        let h = &unpacked[offset..offset + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let sum: usize = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    *b as usize
                }
            })
            .sum();
        let name = field(h, 0, 100)?;
        let size = octal(h, 124, 12)?;
        let end = offset
            .checked_add(512)
            .and_then(|n| n.checked_add(size))
            .context("Tar size overflow")?;
        ensure!(
            sum == octal(h, 148, 8)?
                && matches!(h[156], 0 | 48)
                && field(h, 345, 155)?.is_empty()
                && ["crow", "THIRD_PARTY_NOTICES"].contains(&name)
                && names.insert(name.to_owned())
                && size > 0
                && end <= unpacked.len(),
            "Unexpected or malformed release archive entry."
        );
        if name == "crow" {
            let b = &unpacked[offset + 512..end];
            ensure!(
                b.len() >= 64
                    && &b[..4] == b"\x7fELF"
                    && b[4] == 2
                    && b[5] == 1
                    && u16::from_le_bytes([b[18], b[19]])
                        == if architecture == "x64" { 62 } else { 183 },
                "Release does not contain a Linux {architecture} ELF64 executable."
            );
        }
        offset = end
            .checked_add((512 - size % 512) % 512)
            .context("Tar padding overflow")?;
    }
    ensure!(
        names.len() == 2
            && unpacked.len().saturating_sub(offset) >= 1024
            && unpacked[offset..].iter().all(|b| *b == 0),
        "Incomplete release archive or unexpected trailing data."
    );
    Ok(())
}
struct ReleaseFile {
    name: String,
    bytes: Vec<u8>,
    content_type: &'static str,
}
fn names(directory: &Path) -> Result<BTreeSet<String>> {
    fs::read_dir(directory)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Non-UTF8 release filename"))
        })
        .collect()
}
fn bounded_file(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "File exceeds size limit: {}",
        path.display()
    );
    Ok(bytes)
}
fn assemble(directory: &Path, r: &Request) -> Result<Vec<ReleaseFile>> {
    ensure!(
        names(directory)?
            == ARCHES
                .map(|a| format!("crow-linux-{a}"))
                .into_iter()
                .collect(),
        "Expected exactly the x64 and arm64 build artifacts."
    );
    let mut files = Vec::new();
    let mut sums = String::new();
    for arch in ARCHES {
        let dir = directory.join(format!("crow-linux-{arch}"));
        ensure!(
            fs::symlink_metadata(&dir)?.file_type().is_dir(),
            "Artifact must be a directory"
        );
        let archive = format!("crow-v{}-linux-{arch}.tar.gz", r.version);
        ensure!(
            names(&dir)?
                == [
                    archive.clone(),
                    "SHA256SUMS".into(),
                    "build-metadata.json".into()
                ]
                .into_iter()
                .collect(),
            "Unexpected files in the {arch} artifact."
        );
        let bytes = bounded_file(&dir.join(&archive), MAX_ARCHIVE)?;
        let sha = digest(&bytes);
        let line = format!("{sha}  {archive}\n");
        ensure!(
            bounded_file(&dir.join("SHA256SUMS"), 1024)? == line.as_bytes(),
            "Checksum mismatch for {archive}."
        );
        let m: Value =
            serde_json::from_slice(&bounded_file(&dir.join("build-metadata.json"), 16384)?)?;
        ensure!(
            m["version"] == r.version
                && m["commit"] == r.commit
                && m["arch"] == arch
                && m["archive"] == archive
                && m["sha256"] == sha
                && m["executableVersion"] == r.version,
            "Build metadata does not match {archive} and the selected source commit."
        );
        validate_archive(&bytes, arch)?;
        files.push(ReleaseFile {
            name: archive,
            bytes,
            content_type: "application/gzip",
        });
        sums.push_str(&line);
    }
    files.push(ReleaseFile {
        name: "SHA256SUMS".into(),
        bytes: sums.into_bytes(),
        content_type: "text/plain; charset=utf-8",
    });
    Ok(files)
}
#[async_trait::async_trait]
trait Store {
    async fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn write(
        &mut self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
        immutable: bool,
    ) -> Result<()>;
    async fn public_read(&mut self, key: &str) -> Result<Vec<u8>>;
}
async fn publish(store: &mut impl Store, version: &str, files: &[ReleaseFile]) -> Result<()> {
    ensure!(stable(version), "Invalid release version");
    if let Some(latest) = store.read("latest.txt").await? {
        let text = std::str::from_utf8(&latest)?;
        let old = text
            .strip_suffix('\n')
            .filter(|v| stable(v))
            .context("Existing latest.txt is malformed.")?;
        ensure!(
            compare_versions(old, version) != Ordering::Greater,
            "Refusing to move latest.txt to an older release."
        );
    }
    let mut missing = Vec::new();
    for f in files {
        let key = format!("releases/v{version}/{}", f.name);
        match store.read(&key).await? {
            None => missing.push((key, f)),
            Some(existing) => ensure!(
                existing == f.bytes,
                "Refusing to overwrite changed release object: {key}"
            ),
        }
    }
    for (key, f) in missing {
        store.write(&key, &f.bytes, f.content_type, true).await?;
        println!("Uploaded {key}");
    }
    for f in files {
        let key = format!("releases/v{version}/{}", f.name);
        ensure!(
            store.public_read(&key).await? == f.bytes,
            "Public download verification failed: {key}"
        );
    }
    let latest = format!("{version}\n");
    store
        .write(
            "latest.txt",
            latest.as_bytes(),
            "text/plain; charset=utf-8",
            false,
        )
        .await?;
    ensure!(
        store.public_read("latest.txt").await? == latest.as_bytes(),
        "Release uploaded, but the public latest.txt is stale. Check Cloudflare caching before retrying."
    );
    println!("Published Crow {version}.");
    Ok(())
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?)
}
async fn public_read(client: &reqwest::Client, key: &str) -> Result<Vec<u8>> {
    let mut failure = None;
    for attempt in 0..4 {
        let result = async {
            let mut response = client
                .get(format!("{DOWNLOADS}{key}"))
                .header("Cache-Control", "no-store")
                .send()
                .await?;
            ensure!(
                response.status().is_success(),
                "Public download returned HTTP {}",
                response.status()
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= MAX_ARCHIVE,
                    "Public download exceeds the size limit."
                );
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        }
        .await;
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(error) => failure = Some(error),
        }
        if attempt < 3 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    Err(failure.unwrap())
}
struct R2 {
    account: String,
    bucket: String,
    temporary: tempfile::TempDir,
    sequence: u64,
    client: reqwest::Client,
}
impl R2 {
    async fn new() -> Result<Self> {
        let account = env::var("CLOUDFLARE_ACCOUNT_ID")?;
        let bucket = env::var("CROW_R2_BUCKET").unwrap_or_else(|_| "crow-releases".into());
        ensure!(
            account.len() == 32
                && account
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "Invalid Cloudflare account ID."
        );
        ensure!(
            (3..=63).contains(&bucket.len())
                && bucket
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b".-".contains(&c))
                && bucket.starts_with(|c: char| c.is_ascii_alphanumeric())
                && bucket.ends_with(|c: char| c.is_ascii_alphanumeric()),
            "Invalid R2 bucket name."
        );
        ensure!(
            !env::var("AWS_ACCESS_KEY_ID")?.is_empty()
                && !env::var("AWS_SECRET_ACCESS_KEY")?.is_empty(),
            "R2 S3 credentials are missing."
        );
        let output = tokio::process::Command::new("aws")
            .arg("--version")
            .output()
            .await
            .context("Publication requires the official AWS CLI v2 on PATH.")?;
        ensure!(
            output.status.success()
                && (output.stdout.starts_with(b"aws-cli/2.")
                    || output.stderr.starts_with(b"aws-cli/2.")),
            "Publication requires the official AWS CLI v2."
        );
        Ok(Self {
            account,
            bucket,
            temporary: tempfile::tempdir()?,
            sequence: 0,
            client: client()?,
        })
    }
    async fn command(&self, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
        let mut cmd = tokio::process::Command::new("aws");
        cmd.arg("s3api")
            .args(args)
            .args([
                "--bucket",
                &self.bucket,
                "--endpoint-url",
                &format!("https://{}.r2.cloudflarestorage.com", self.account),
                "--region",
                "auto",
                "--no-cli-pager",
            ])
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_PAGER", "")
            .env("AWS_REQUEST_CHECKSUM_CALCULATION", "when_required")
            .env("AWS_RESPONSE_CHECKSUM_VALIDATION", "when_required")
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(180), cmd.output())
            .await
            .context("AWS CLI timed out")?
            .context("AWS CLI failed")
    }
    fn path(&mut self) -> PathBuf {
        self.sequence += 1;
        self.temporary.path().join(self.sequence.to_string())
    }
}
#[async_trait::async_trait]
impl Store for R2 {
    async fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path();
        let result = self
            .command(&[
                "get-object".as_ref(),
                "--key".as_ref(),
                key.as_ref(),
                path.as_os_str(),
            ])
            .await?;
        if !result.status.success() {
            let error = String::from_utf8_lossy(&result.stderr);
            if error.contains("An error occurred (NoSuchKey) when calling the GetObject operation")
                || error.contains("An error occurred (404) when calling the GetObject operation")
            {
                return Ok(None);
            }
            bail!("R2 read failed: {error}");
        }
        let bytes = bounded_file(&path, MAX_ARCHIVE)?;
        fs::remove_file(path)?;
        Ok(Some(bytes))
    }
    async fn write(
        &mut self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
        immutable: bool,
    ) -> Result<()> {
        let path = self.path();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        drop(file);
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "put-object".as_ref(),
            "--key".as_ref(),
            key.as_ref(),
            "--body".as_ref(),
            path.as_os_str(),
            "--content-type".as_ref(),
            content_type.as_ref(),
            "--cache-control".as_ref(),
            if immutable {
                "public, max-age=31536000, immutable".as_ref()
            } else {
                "no-store, max-age=0".as_ref()
            },
        ];
        if immutable {
            args.push("--if-none-match".as_ref());
            args.push("*".as_ref());
        }
        let result = self.command(&args).await?;
        fs::remove_file(path)?;
        ensure!(
            result.status.success(),
            "R2 write failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    }
    async fn public_read(&mut self, key: &str) -> Result<Vec<u8>> {
        public_read(&self.client, key).await
    }
}
fn checked_output(command: &mut Command) -> Result<Vec<u8>> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "Command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
fn license_files(directory: &Path, depth: u8) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory)? {
        let e = entry?;
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if e.file_type()?.is_file()
            && (name.starts_with("license")
                || name.starts_with("licence")
                || name.starts_with("copying")
                || name.starts_with("copyright")
                || name.starts_with("notice"))
        {
            found.push(e.path());
        } else if depth > 0
            && e.file_type()?.is_dir()
            && ["licenses", "licences", "license"].contains(&name.as_str())
        {
            found.extend(license_files(&e.path(), depth - 1)?);
        }
    }
    found.sort();
    Ok(found)
}
fn notices() -> Result<String> {
    let compiler = String::from_utf8(checked_output(Command::new("rustc").arg("-vV"))?)?;
    let host = compiler
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("Missing rustc host target")?;
    let metadata: Value = serde_json::from_slice(&checked_output(Command::new("cargo").args([
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        host,
    ]))?)?;
    let resolved: BTreeSet<&str> = metadata["resolve"]["nodes"]
        .as_array()
        .context("Missing Cargo dependency graph")?
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    let mut result = String::from(
        "Crow includes Rust libraries and the Rust standard library.\nDependency license declarations and distributed notices follow.\n",
    );
    for package in metadata["packages"]
        .as_array()
        .context("Missing Cargo packages")?
    {
        if package["source"].is_null()
            || !resolved.contains(package["id"].as_str().unwrap_or_default())
        {
            continue;
        }
        let manifest = Path::new(
            package["manifest_path"]
                .as_str()
                .context("Missing Cargo manifest path")?,
        );
        let directory = manifest.parent().context("Missing crate directory")?;
        result.push_str(&format!(
            "\n===== {} {} ({}) =====\n",
            package["name"].as_str().unwrap_or("unknown"),
            package["version"].as_str().unwrap_or("unknown"),
            package["license"].as_str().unwrap_or("see license file")
        ));
        let mut files = license_files(directory, 2)?;
        if let Some(file) = package["license_file"].as_str() {
            let p = directory.join(file);
            if !files.contains(&p) {
                files.push(p);
            }
        }
        ensure!(
            !files.is_empty(),
            "Crate {} contains no distributable license text; add its upstream notice before release",
            package["name"]
        );
        for file in files {
            result.push_str(&format!(
                "\n--- {} ---\n",
                file.strip_prefix(directory)?.display()
            ));
            result.push_str(&fs::read_to_string(&file)?);
            result.push('\n');
        }
    }
    let sysroot = String::from_utf8(checked_output(
        Command::new("rustc").args(["--print", "sysroot"]),
    )?)?;
    let directory = Path::new(sysroot.trim()).join("share/doc/rust");
    let files = license_files(&directory, 2)
        .context("Install the rust-docs toolchain component to package standard-library notices")?;
    ensure!(
        !files.is_empty(),
        "Rust standard-library license notices are missing"
    );
    result.push_str("\n===== Rust standard library =====\n");
    for file in files {
        // Compiler/tooling notices are not part of the shipped executable.
        if file
            .file_name()
            .is_some_and(|name| name == "COPYRIGHT.html")
            && directory.join("COPYRIGHT-library.html").is_file()
        {
            continue;
        }
        result.push_str(&format!(
            "\n--- {} ---\n",
            file.strip_prefix(&directory)?.display()
        ));
        result.push_str(&fs::read_to_string(file)?);
        result.push('\n');
    }
    Ok(result)
}
fn pack(executable: &Path, out: &Path) -> Result<()> {
    ensure!(
        env::consts::OS == "linux",
        "Build release binaries natively on Linux."
    );
    let arch = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => bail!("Unsupported release architecture"),
    };
    let version = env!("CARGO_PKG_VERSION");
    ensure!(stable(version), "Release version must be stable");
    if let Ok(tag) = env::var("RELEASE_TAG") {
        ensure!(
            tag == format!("v{version}"),
            "Tag must match Cargo.toml version"
        );
    }
    let commit = env::var("GITHUB_SHA")
        .or_else(|_| {
            String::from_utf8(
                checked_output(Command::new("git").args(["rev-parse", "HEAD"]))
                    .map_err(|_| env::VarError::NotPresent)?,
            )
            .map_err(|_| env::VarError::NotPresent)
        })?
        .trim()
        .to_string();
    ensure!(full_commit(&commit), "Invalid source commit");
    let actual = String::from_utf8(checked_output(Command::new(executable).arg("--version"))?)?;
    ensure!(
        actual.trim() == version,
        "Packaged executable version mismatch"
    );
    let binary = bounded_file(executable, MAX_UNPACKED)?;
    let notices = notices()?;
    fs::create_dir_all(out)?;
    let stage = tempfile::tempdir_in(out)?;
    let archive = format!("crow-v{version}-linux-{arch}.tar.gz");
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(encoder);
    for (name, bytes, mode) in [
        ("crow", binary.as_slice(), 0o755),
        ("THIRD_PARTY_NOTICES", notices.as_bytes(), 0o644),
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_path(name)?;
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        tar.append(&header, bytes)?;
    }
    let bytes = tar.into_inner()?.finish()?;
    validate_archive(&bytes, arch)?;
    let sha = digest(&bytes);
    fs::write(stage.path().join(&archive), bytes)?;
    fs::write(
        stage.path().join("SHA256SUMS"),
        format!("{sha}  {archive}\n"),
    )?;
    fs::write(
        stage.path().join("build-metadata.json"),
        format!(
            "{}\n",
            json!({"version":version,"executableVersion":actual.trim(),"arch":arch,"archive":archive,"sha256":sha,"commit":commit})
        ),
    )?;
    for name in [&archive, "SHA256SUMS", "build-metadata.json"] {
        fs::rename(stage.path().join(name), out.join(name))?;
    }
    println!("Built {}", out.join(archive).display());
    Ok(())
}
async fn verify_run(r: &Request) -> Result<()> {
    let repository = env::var("GITHUB_REPOSITORY")?;
    ensure!(
        repository.split('/').count() == 2
            && repository.split('/').all(|p| !p.is_empty()
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))),
        "Invalid GitHub repository"
    );
    let response = client()?
        .get(format!(
            "https://api.github.com/repos/{repository}/actions/runs/{}",
            r.run_id
        ))
        .bearer_auth(env::var("GH_TOKEN")?)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "crow-maint")
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Cannot verify build run: HTTP {}",
        response.status()
    );
    validate_run(&response.json().await?, r, &repository)?;
    println!("Verified build {} at {}.", r.run_id, r.commit);
    Ok(())
}
async fn prepare_site() -> Result<()> {
    ensure!(
        env::var("PREPARE_CONFIRMATION").as_deref() == Ok("create crow-site"),
        "Explicit project creation confirmation is required"
    );
    let account = env::var("CLOUDFLARE_ACCOUNT_ID")?;
    let token = env::var("CLOUDFLARE_API_TOKEN")?;
    ensure!(
        account.len() == 32
            && account
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            && !token.is_empty(),
        "Cloudflare account ID or Pages token is missing or invalid"
    );
    let base = format!("https://api.cloudflare.com/client/v4/accounts/{account}/pages/projects");
    let c = client()?;
    let mut response = c
        .get(format!("{base}/crow-site"))
        .bearer_auth(&token)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        response = c
            .post(&base)
            .bearer_auth(&token)
            .json(&json!({"name":"crow-site","production_branch":"main"}))
            .send()
            .await?;
    }
    ensure!(
        response.status().is_success(),
        "Cloudflare Pages request failed with HTTP {}; check the account ID and Pages Edit permission",
        response.status()
    );
    let body: Value = response.json().await?;
    let project = &body["result"];
    ensure!(
        body["success"] == true
            && project["name"] == "crow-site"
            && project["production_branch"] == "main"
            && project["source"].is_null(),
        "Expected crow-site with production branch main and no Git integration; inspect the existing project before continuing"
    );
    summary(
        "The crow-site Pages project is ready. No site files were uploaded.\n\nNext, open crow-site in Cloudflare, choose Custom domains, and add birdapp.dev.\n",
    )
}
async fn stage_site(directory: &Path) -> Result<()> {
    ensure!(
        env::var("DEPLOY_CONFIRMATION").as_deref() == Ok("deploy birdapp.dev"),
        "Explicit site deployment confirmation is required"
    );
    ensure!(
        directory.join("install.sh").is_file(),
        "Installation script missing"
    );
    let c = client()?;
    let latest = public_read(&c, "latest.txt").await?;
    let text = std::str::from_utf8(&latest)?;
    let version = text
        .strip_suffix('\n')
        .filter(|v| stable(v))
        .context("Public latest.txt is malformed")?;
    let mut objects = ARCHES
        .map(|arch| format!("crow-v{version}-linux-{arch}.tar.gz"))
        .to_vec();
    objects.push("SHA256SUMS".into());
    for name in objects {
        let response = c
            .head(format!("{DOWNLOADS}releases/v{version}/{name}"))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "Public release object unavailable: {name}"
        );
    }
    let path = directory.join("index.html");
    let html = fs::read_to_string(&path)?;
    ensure!(
        html.contains("__CROW_VERSION__"),
        "Installation page is missing its version placeholder"
    );
    fs::write(path, html.replace("__CROW_VERSION__", version))?;
    Ok(())
}
fn summary(message: &str) -> Result<()> {
    if let Ok(path) = env::var("GITHUB_STEP_SUMMARY") {
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?
            .write_all(message.as_bytes())?;
    } else {
        println!("{message}");
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Action::Pack { executable, out } => pack(&executable, &out),
        Action::VerifyRun => verify_run(&release_request()?).await,
        Action::Publish { directory } => {
            let r = release_request()?;
            let files = assemble(&directory, &r)?;
            publish(&mut R2::new().await?, &r.version, &files).await?;
            summary(&format!(
                "Published Crow {}\n\nSource commit: {}\n\nBuild run: {}\n\n{DOWNLOADS}latest.txt\n",
                r.version, r.commit, r.run_id
            ))
        }
        Action::PrepareSite => prepare_site().await,
        Action::StageSite { directory } => stage_site(&directory).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn request_fixture() -> Request {
        request(
            "0.2.0".into(),
            "a".repeat(40),
            "123".into(),
            "publish 0.2.0",
        )
        .unwrap()
    }
    fn archive(arch: &str) -> Vec<u8> {
        let mut binary = vec![0; 64];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[18..20].copy_from_slice(&(if arch == "x64" { 62u16 } else { 183u16 }).to_le_bytes());
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, bytes) in [
            ("crow", binary.as_slice()),
            ("THIRD_PARTY_NOTICES", b"License".as_slice()),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_path(name).unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }
    fn files() -> Vec<ReleaseFile> {
        vec![
            ReleaseFile {
                name: "x64.tar.gz".into(),
                bytes: b"x64".to_vec(),
                content_type: "application/gzip",
            },
            ReleaseFile {
                name: "arm64.tar.gz".into(),
                bytes: b"arm64".to_vec(),
                content_type: "application/gzip",
            },
            ReleaseFile {
                name: "SHA256SUMS".into(),
                bytes: b"checksums".to_vec(),
                content_type: "text/plain",
            },
        ]
    }
    #[derive(Default)]
    struct FakeStore {
        data: BTreeMap<String, Vec<u8>>,
        events: Vec<String>,
        corrupt: bool,
        interrupt: bool,
    }
    #[async_trait::async_trait]
    impl Store for FakeStore {
        async fn read(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.data.get(key).cloned())
        }
        async fn write(&mut self, key: &str, bytes: &[u8], _: &str, immutable: bool) -> Result<()> {
            if self.interrupt && key.ends_with("arm64.tar.gz") {
                bail!("interrupted")
            }
            ensure!(
                !immutable || !self.data.contains_key(key),
                "Cannot replace immutable object"
            );
            self.events.push(format!("write:{key}"));
            self.data.insert(key.into(), bytes.to_vec());
            Ok(())
        }
        async fn public_read(&mut self, key: &str) -> Result<Vec<u8>> {
            self.events.push(format!("verify:{key}"));
            Ok(if self.corrupt {
                b"wrong".to_vec()
            } else {
                self.data[key].clone()
            })
        }
    }
    #[test]
    fn explicit_request_and_exact_repository_build_required() {
        let r = request_fixture();
        for v in ["00.2.0", "0.2.0-beta", "0.2", "0.2.0\n"] {
            assert!(
                request(
                    v.into(),
                    r.commit.clone(),
                    r.run_id.clone(),
                    &format!("publish {v}")
                )
                .is_err()
            );
        }
        assert!(request(r.version.clone(), r.commit.clone(), r.run_id.clone(), "yes").is_err());
        assert!(
            request(
                r.version.clone(),
                "main".into(),
                r.run_id.clone(),
                "publish 0.2.0"
            )
            .is_err()
        );
        assert!(
            request(
                r.version.clone(),
                r.commit.clone(),
                "123/other".into(),
                "publish 0.2.0"
            )
            .is_err()
        );
        let run = json!({"id":123,"repository":{"full_name":"Byntham/Crow"},"head_repository":{"full_name":"Byntham/Crow"},"head_sha":r.commit,"path":".github/workflows/release.yml","status":"completed","conclusion":"success","event":"workflow_dispatch"});
        validate_run(&run, &r, "Byntham/Crow").unwrap();
        for (key, value) in [
            ("id", json!(124)),
            ("head_sha", json!("b".repeat(40))),
            ("path", json!("other.yml")),
            ("conclusion", json!("failure")),
            ("event", json!("pull_request")),
            ("head_repository", json!({"full_name":"fork/Crow"})),
        ] {
            let mut changed = run.clone();
            changed[key] = value;
            assert!(validate_run(&changed, &r, "Byntham/Crow").is_err());
        }
    }
    #[test]
    fn validates_archive_architecture_headers_and_trailing_bytes() {
        for arch in ARCHES {
            validate_archive(&archive(arch), arch).unwrap();
        }
        assert!(
            validate_archive(&archive("arm64"), "x64")
                .unwrap_err()
                .to_string()
                .contains("ELF64")
        );
        let mut raw = Vec::new();
        MultiGzDecoder::new(archive("x64").as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        raw[0] = b'x';
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&raw).unwrap();
        assert!(validate_archive(&gz.finish().unwrap(), "x64").is_err());
        let mut bytes = archive("x64");
        bytes.extend(b"trailing garbage");
        assert!(validate_archive(&bytes, "x64").is_err());
        assert_eq!(
            compare_versions("999999999999999999999.0.0", "123.0.0"),
            Ordering::Greater
        );
    }
    #[test]
    fn assembly_checks_hashes_source_and_exact_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let r = request_fixture();
        for arch in ARCHES {
            let path = dir.path().join(format!("crow-linux-{arch}"));
            fs::create_dir(&path).unwrap();
            let name = format!("crow-v{}-linux-{arch}.tar.gz", r.version);
            let bytes = archive(arch);
            let sha = digest(&bytes);
            fs::write(path.join(&name), bytes).unwrap();
            fs::write(path.join("SHA256SUMS"), format!("{sha}  {name}\n")).unwrap();
            fs::write(path.join("build-metadata.json"),json!({"version":r.version,"executableVersion":r.version,"commit":r.commit,"arch":arch,"archive":name,"sha256":sha}).to_string()).unwrap();
        }
        assert_eq!(assemble(dir.path(), &r).unwrap().len(), 3);
        let mut changed = r.clone();
        changed.commit = "b".repeat(40);
        assert!(
            assemble(dir.path(), &changed)
                .err()
                .unwrap()
                .to_string()
                .contains("metadata")
        );
        fs::write(dir.path().join("crow-linux-x64/SHA256SUMS"), "wrong").unwrap();
        assert!(
            assemble(dir.path(), &r)
                .err()
                .unwrap()
                .to_string()
                .contains("Checksum")
        );
    }
    #[tokio::test]
    async fn public_verification_precedes_latest_and_retries_do_not_replace_releases() {
        let mut store = FakeStore::default();
        publish(&mut store, "0.2.0", &files()).await.unwrap();
        assert_eq!(
            &store.events[store.events.len() - 5..],
            [
                "verify:releases/v0.2.0/x64.tar.gz",
                "verify:releases/v0.2.0/arm64.tar.gz",
                "verify:releases/v0.2.0/SHA256SUMS",
                "write:latest.txt",
                "verify:latest.txt"
            ]
        );
        store.events.clear();
        publish(&mut store, "0.2.0", &files()).await.unwrap();
        assert_eq!(
            store
                .events
                .iter()
                .filter(|s| s.starts_with("write:"))
                .collect::<Vec<_>>(),
            ["write:latest.txt"]
        );
    }
    #[tokio::test]
    async fn rejects_changed_release_downgrade_and_public_corruption() {
        let mut store = FakeStore::default();
        store
            .data
            .insert("releases/v0.2.0/SHA256SUMS".into(), b"old".to_vec());
        assert!(publish(&mut store, "0.2.0", &files()).await.is_err());
        assert!(store.events.is_empty());
        let mut store = FakeStore::default();
        store.data.insert("latest.txt".into(), b"0.3.0\n".to_vec());
        assert!(publish(&mut store, "0.2.0", &files()).await.is_err());
        assert!(store.events.is_empty());
        let mut store = FakeStore {
            corrupt: true,
            ..Default::default()
        };
        store.data.insert("latest.txt".into(), b"0.1.0\n".to_vec());
        assert!(publish(&mut store, "0.2.0", &files()).await.is_err());
        assert_eq!(store.data["latest.txt"], b"0.1.0\n");
    }
    #[tokio::test]
    async fn interrupted_upload_resumes_without_overwriting_existing_objects() {
        let mut store = FakeStore {
            interrupt: true,
            ..Default::default()
        };
        assert!(publish(&mut store, "0.2.0", &files()).await.is_err());
        assert!(!store.data.contains_key("latest.txt"));
        store.interrupt = false;
        store.events.clear();
        publish(&mut store, "0.2.0", &files()).await.unwrap();
        assert!(
            !store
                .events
                .iter()
                .any(|e| e == "write:releases/v0.2.0/x64.tar.gz")
        );
        assert_eq!(store.data["latest.txt"], b"0.2.0\n");
    }
}
