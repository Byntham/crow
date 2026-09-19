//! Read-only inspection of pinned Git objects. Reviewed files are never checked out.
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct InspectionError {
    pub kind: String,
    pub message: String,
}

const BLOB_LIMIT: usize = 2 * 1024 * 1024;
const GUIDANCE_LIMIT: usize = 256_000;

pub fn revision(value: &str) -> Result<&str> {
    ensure!(
        (40..=64).contains(&value.len())
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "Invalid Git revision"
    );
    Ok(value)
}
pub fn safe_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains(['\\', '\0', '\r', '\n'])
        && !value.split('/').any(|p| p == ".." || p.is_empty())
}
fn path_arg(value: &Value) -> Result<&str> {
    let path = value
        .as_str()
        .ok_or_else(|| anyhow!("Invalid repository path"))?;
    ensure!(safe_path(path), "Invalid repository path");
    Ok(path)
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing inspection field {key}"))
}
fn source_dir(source: &Value) -> Result<&Path> {
    Ok(Path::new(field(source, "dir")?))
}
fn source_rev<'a>(source: &'a Value, key: &str) -> Result<&'a str> {
    revision(field(source, key)?)
}

// Kill the process group even if the async operation is dropped by its caller.
struct GitGuard {
    pid: u32,
    armed: bool,
}
impl Drop for GitGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Git exited with status {code}: {message}")]
struct GitFailure {
    code: i32,
    message: String,
}

async fn git_stream<F>(
    dir: &Path,
    args: &[String],
    extra: &BTreeMap<String, String>,
    cancel: CancellationToken,
    mut consume: F,
) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    ensure!(!cancel.is_cancelled(), "Inspection cancelled");
    let dir = std::fs::canonicalize(dir).context("Invalid Git repository directory")?;
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "credential.helper=",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(&dir)
        .args(args)
        .current_dir(&dir)
        .env_clear()
        .envs(crate::util::clean_env())
        .envs(extra)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn().context("Cannot start Git")?;
    let mut guard = GitGuard {
        pid: child.id().unwrap_or(0),
        armed: true,
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let output = async {
        let read_out = async {
            let mut buffer = [0u8; 16_384];
            loop {
                let n = stdout.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                consume(&buffer[..n])?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let read_err = async {
            let mut result = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stderr.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                let keep = n.min(65_536usize.saturating_sub(result.len()));
                result.extend_from_slice(&buffer[..keep]);
            }
            Ok::<_, anyhow::Error>(result)
        };
        let (_, err) = tokio::try_join!(read_out, read_err)?;
        let status = child.wait().await?;
        if !status.success() {
            return Err(GitFailure {
                code: status.code().unwrap_or(-1),
                message: String::from_utf8_lossy(&err).trim().to_owned(),
            }
            .into());
        }
        Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("Inspection cancelled")),
        result = tokio::time::timeout(Duration::from_secs(120), output) => result.unwrap_or_else(|_| Err(anyhow!("Git timed out"))),
    };
    if result.is_err() {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(guard.pid as i32), libc::SIGKILL);
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    guard.armed = false;
    result
}
async fn git_bytes(
    dir: &Path,
    args: &[String],
    limit: usize,
    extra: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    git_stream(dir, args, extra, cancel, |chunk| {
        ensure!(
            output.len().saturating_add(chunk.len()) <= limit,
            "Git output exceeds {limit} bytes"
        );
        output.extend_from_slice(chunk);
        Ok(())
    })
    .await?;
    Ok(output)
}
pub async fn git(dir: &Path, args: &[String]) -> Result<String> {
    Ok(String::from_utf8_lossy(
        &git_bytes(
            dir,
            args,
            16 * 1024 * 1024,
            &BTreeMap::new(),
            CancellationToken::new(),
        )
        .await?,
    )
    .into_owned())
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

/// Export pinned source without consulting repository export-ignore/export-subst
/// attributes. The archive is passed to the container, never unpacked on the host.
pub async fn execution_archive(
    source: &Value,
    revision_key: &str,
    output: &Path,
    max_bytes: u64,
    cancel: CancellationToken,
) -> Result<()> {
    use std::io::Write;
    let attributes = tempfile::tempdir()?;
    let mut file = std::fs::File::create(output)?;
    let mut total = 0u64;
    git_stream(
        source_dir(source)?,
        &strings(&[
            "-c",
            "core.bare=false",
            "archive",
            "--worktree-attributes",
            "--format=tar",
            source_rev(source, revision_key)?,
        ]),
        &BTreeMap::from([
            (
                "GIT_WORK_TREE".into(),
                attributes.path().to_string_lossy().into_owned(),
            ),
            (
                "GIT_INDEX_FILE".into(),
                attributes
                    .path()
                    .join("empty-index")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]),
        cancel,
        |chunk| {
            total += chunk.len() as u64;
            ensure!(
                total <= max_bytes,
                "Execution source archive exceeds {max_bytes} bytes"
            );
            file.write_all(chunk)?;
            Ok(())
        },
    )
    .await
}

pub async fn checkout(
    root: &Path,
    job: &Value,
    pr: &Value,
    token: &str,
    cancel: CancellationToken,
) -> Result<Value> {
    let id = field(job, "id")?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Invalid job ID"
    );
    let repo = field(job, "repo")?;
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|s| !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "Invalid repository"
    );
    let number = job["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("Invalid pull request number"))?;
    let head = revision(field(&pr["head"], "sha")?)?;
    let target = revision(field(&pr["base"], "sha")?)?;
    let dir = root.join("sources").join(id);
    tokio::fs::create_dir_all(&dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    git_bytes(
        &dir,
        &strings(&["init", "--bare", "."]),
        BLOB_LIMIT,
        &BTreeMap::new(),
        cancel.clone(),
    )
    .await?;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let extra = BTreeMap::from([
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        (
            "GIT_CONFIG_KEY_0".into(),
            "http.https://github.com/.extraheader".into(),
        ),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!("AUTHORIZATION: basic {auth}"),
        ),
    ]);
    git_bytes(
        &dir,
        &strings(&[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            &format!("https://github.com/{repo}.git"),
            target,
            &format!("refs/pull/{number}/head:refs/crow/head"),
        ]),
        BLOB_LIMIT,
        &extra,
        cancel.clone(),
    )
    .await?;
    let actual = git_bytes(
        &dir,
        &strings(&["rev-parse", "refs/crow/head"]),
        1024,
        &BTreeMap::new(),
        cancel.clone(),
    )
    .await?;
    if String::from_utf8_lossy(&actual).trim() != head {
        return Err(InspectionError {
            kind: "superseded".into(),
            message: "PR changed while fetching".into(),
        }
        .into());
    }
    let base = git_bytes(
        &dir,
        &strings(&["merge-base", head, target]),
        1024,
        &BTreeMap::new(),
        cancel,
    )
    .await?;
    let base = String::from_utf8_lossy(&base).trim().to_owned();
    revision(&base)?;
    Ok(
        json!({"dir":dir,"head":head,"base":base,"target":field(&pr["base"],"ref")?,"targetSha":target}),
    )
}

async fn names<F>(source: &Value, args: &[String], visit: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    names_with_cancel(source, args, CancellationToken::new(), visit).await
}
async fn names_with_cancel<F>(
    source: &Value,
    args: &[String],
    cancel: CancellationToken,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let mut pending = Vec::new();
    git_stream(
        source_dir(source)?,
        args,
        &BTreeMap::new(),
        cancel,
        |chunk| {
            for part in chunk.split_inclusive(|b| *b == 0) {
                let complete = part.last() == Some(&0);
                let text = if complete {
                    &part[..part.len() - 1]
                } else {
                    part
                };
                ensure!(
                    pending.len().saturating_add(text.len()) <= 65_536,
                    "Repository path exceeds 64 KB"
                );
                pending.extend_from_slice(text);
                if complete {
                    if !pending.is_empty() {
                        visit(&String::from_utf8_lossy(&pending))?;
                    }
                    pending.clear();
                }
            }
            Ok(())
        },
    )
    .await?;
    ensure!(pending.is_empty(), "Incomplete Git path listing");
    Ok(())
}
fn tree_args(rev: &str) -> Result<Vec<String>> {
    Ok(strings(&[
        "ls-tree",
        "-r",
        "--name-only",
        "-z",
        revision(rev)?,
    ]))
}
fn diff_args(source: &Value, path: Option<&str>) -> Result<Vec<String>> {
    let mut args = strings(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--unified=5",
        source_rev(source, "base")?,
        source_rev(source, "head")?,
    ]);
    if let Some(path) = path {
        ensure!(safe_path(path), "Invalid repository path");
        args.extend(strings(&["--", path]));
    }
    Ok(args)
}
pub async fn read_blob(source: &Value, path: &str, rev: Option<&str>) -> Result<String> {
    read_blob_with_cancel(source, path, rev, CancellationToken::new()).await
}
async fn read_blob_with_cancel(
    source: &Value,
    path: &str,
    rev: Option<&str>,
    cancel: CancellationToken,
) -> Result<String> {
    ensure!(safe_path(path), "Invalid repository path");
    let rev = revision(rev.unwrap_or(source_rev(source, "head")?))?;
    let meta = git_bytes(
        source_dir(source)?,
        &strings(&["ls-tree", "-z", rev, "--", path]),
        131_072,
        &BTreeMap::new(),
        cancel.clone(),
    )
    .await?;
    // NUL-delimited output preserves tabs, quotes and other unusual filenames.
    let entry = meta
        .strip_suffix(&[0])
        .ok_or_else(|| anyhow!("Only regular tracked files can be read"))?;
    let tab = entry
        .iter()
        .position(|b| *b == b'\t')
        .ok_or_else(|| anyhow!("Only regular tracked files can be read"))?;
    ensure!(
        &entry[tab + 1..] == path.as_bytes(),
        "Only regular tracked files can be read"
    );
    let header = std::str::from_utf8(&entry[..tab])?;
    let columns: Vec<_> = header.split(' ').collect();
    ensure!(
        columns.len() == 3 && ["100644", "100755"].contains(&columns[0]) && columns[1] == "blob",
        "Only regular tracked files can be read"
    );
    let oid = revision(columns[2])?;
    let text = git_bytes(
        source_dir(source)?,
        &strings(&["cat-file", "blob", oid]),
        BLOB_LIMIT,
        &BTreeMap::new(),
        cancel,
    )
    .await?;
    ensure!(!text.contains(&0), "Binary file cannot be read as text");
    Ok(String::from_utf8_lossy(&text).into_owned())
}
fn json_integer(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && v.fract() == 0.0 && *v >= 0.0 && *v <= 9_007_199_254_740_991.0)
        .map(|v| v as u64)
}
fn bounds(args: &Value, maximum: usize) -> Result<(usize, usize)> {
    let offset = args
        .get("offset")
        .filter(|v| !v.is_null())
        .map_or(Some(0), json_integer);
    let count = args
        .get("count")
        .filter(|v| !v.is_null())
        .map_or(Some(maximum as u64), json_integer);
    let error = || {
        anyhow!("offset must be a nonnegative integer and count must be between 1 and {maximum}")
    };
    let (offset, count) = (offset.ok_or_else(error)?, count.ok_or_else(error)?);
    ensure!(
        offset <= 9_007_199_254_740_991 && count > 0 && count <= maximum as u64,
        "offset must be a nonnegative integer and count must be between 1 and {maximum}"
    );
    Ok((usize::try_from(offset)?, count as usize))
}
fn page(total: usize, offset: usize, count: usize) -> Value {
    let next = offset.saturating_add(count);
    let more = next < total;
    json!({"offset":offset,"total":total,"nextOffset":if more {Some(next)} else {None},"truncated":more})
}
// Preserve UTF-8 code points split across operating-system reads, while retaining
// only a small trailing fragment. Diff pagination counts Unicode scalar values.
#[derive(Default)]
struct TextStream {
    pending: Vec<u8>,
}
impl TextStream {
    fn feed(&mut self, bytes: &[u8], mut visit: impl FnMut(&str)) {
        self.pending.extend_from_slice(bytes);
        let mut used = 0;
        while used < self.pending.len() {
            match std::str::from_utf8(&self.pending[used..]) {
                Ok(s) => {
                    visit(s);
                    used = self.pending.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        visit(
                            std::str::from_utf8(&self.pending[used..used + valid])
                                .expect("valid prefix"),
                        );
                        used += valid;
                    }
                    match e.error_len() {
                        Some(n) => {
                            visit("\u{fffd}");
                            used += n;
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..used);
    }
    fn finish(&mut self, mut visit: impl FnMut(&str)) {
        if !self.pending.is_empty() {
            visit("\u{fffd}");
            self.pending.clear();
        }
    }
}
pub async fn diff(
    source: &Value,
    path: Option<&str>,
    offset: usize,
    count: usize,
    cancel: CancellationToken,
) -> Result<Value> {
    bounds(&json!({"offset":offset,"count":count}), 200_000)?;
    let mut patch = String::new();
    let mut total = 0usize;
    let mut decoder = TextStream::default();
    let mut visit = |text: &str| {
        for c in text.chars() {
            if total >= offset && total - offset < count {
                patch.push(c);
            }
            total += 1;
        }
    };
    git_stream(
        source_dir(source)?,
        &diff_args(source, path)?,
        &BTreeMap::new(),
        cancel,
        |chunk| {
            decoder.feed(chunk, &mut visit);
            Ok(())
        },
    )
    .await?;
    decoder.finish(&mut visit);
    let mut result = page(total, offset, count);
    result["patch"] = patch.into();
    Ok(result)
}
pub async fn publication_patch(
    source: &Value,
    findings: &Value,
    cancel: CancellationToken,
) -> Result<String> {
    let mut wanted: Vec<(String, BTreeSet<u64>)> = Vec::new();
    for finding in findings
        .as_array()
        .ok_or_else(|| anyhow!("Invalid findings"))?
    {
        let path = path_arg(&finding["path"])?;
        let line = finding["line"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow!("Invalid finding line"))?;
        if let Some((_, lines)) = wanted.iter_mut().find(|(p, _)| p == path) {
            lines.insert(line);
        } else {
            wanted.push((path.to_owned(), BTreeSet::from([line])));
        }
    }
    let hunk = regex::Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")?;
    let mut result = String::new();
    for (path, lines) in wanted {
        let mut prefix = Vec::new();
        let mut line = 0u64;
        let mut in_hunk = false;
        let mut added = BTreeSet::new();
        let mut consume = |prefix: &[u8]| {
            let text = String::from_utf8_lossy(prefix);
            if let Some(caps) = hunk.captures(&text) {
                line = caps[1].parse().unwrap_or(0);
                in_hunk = true;
            } else if in_hunk {
                match prefix.first() {
                    Some(b'+') => {
                        if lines.contains(&line) {
                            added.insert(line);
                        }
                        line = line.saturating_add(1);
                    }
                    Some(b' ') => line = line.saturating_add(1),
                    Some(b'-' | b'\\') => (),
                    _ => in_hunk = false,
                }
            }
        };
        git_stream(
            source_dir(source)?,
            &diff_args(source, Some(&path))?,
            &BTreeMap::new(),
            cancel.clone(),
            |chunk| {
                for part in chunk.split_inclusive(|b| *b == b'\n') {
                    let complete = part.last() == Some(&b'\n');
                    let n = if complete { part.len() - 1 } else { part.len() };
                    prefix.extend_from_slice(&part[..n.min(256usize.saturating_sub(prefix.len()))]);
                    if complete {
                        consume(&prefix);
                        prefix.clear();
                    }
                }
                Ok(())
            },
        )
        .await?;
        if !prefix.is_empty() {
            consume(&prefix);
        }
        if !added.is_empty() {
            result.push_str(&format!("+++ b/{path}\n"));
            for line in added {
                result.push_str(&format!("@@ -0,0 +{line},1 @@\n+\n"));
            }
        }
    }
    Ok(result)
}
pub async fn guidance(source: &Value) -> Result<Value> {
    guidance_with_cancel(source, CancellationToken::new()).await
}
pub async fn guidance_with_cancel(source: &Value, cancel: CancellationToken) -> Result<Value> {
    let mut wanted = Vec::new();
    let mut size = 0;
    names_with_cancel(
        source,
        &tree_args(source_rev(source, "targetSha")?)?,
        cancel.clone(),
        |path| {
            if path == "AGENTS.md" || path.ends_with("/AGENTS.md") || path == ".crow/review.md" {
                size += path.len();
                ensure!(
                    size <= GUIDANCE_LIMIT,
                    "Repository guidance exceeds 256 KB; reduce the instruction files"
                );
                wanted.push(path.to_owned());
            }
            Ok(())
        },
    )
    .await?;
    // JavaScript sorted UTF-16 code units; retain persisted fingerprint order.
    wanted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut files = Vec::new();
    for path in wanted {
        let body = read_blob_with_cancel(
            source,
            &path,
            Some(source_rev(source, "targetSha")?),
            cancel.clone(),
        )
        .await?;
        size += body.len();
        ensure!(
            size <= GUIDANCE_LIMIT,
            "Repository guidance exceeds 256 KB; reduce the instruction files"
        );
        // Field order is part of the persisted guidance fingerprint.
        let mut entry = serde_json::Map::new();
        entry.insert("path".into(), path.into());
        entry.insert("body".into(), body.into());
        files.push(Value::Object(entry));
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&files)?));
    Ok(json!({"files":files,"fingerprint":fingerprint}))
}
pub async fn inspection_tool(source: &Value, name: &str, args: &Value) -> Result<Value> {
    ensure!(args.is_object(), "Inspection arguments must be an object");
    match name {
        "list_files" => {
            let (offset, count) = bounds(args, 10_000)?;
            ensure!(
                args.get("prefix").is_none_or(Value::is_string),
                "prefix must be a string"
            );
            ensure!(
                args.get("changed_only").is_none_or(Value::is_boolean),
                "changed_only must be a boolean"
            );
            let prefix = args["prefix"].as_str().unwrap_or("");
            let mut files = Vec::new();
            let mut total = 0;
            let mut chars = 0;
            let mut filled = false;
            let cmd = if args["changed_only"] == true {
                strings(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--name-only",
                    "-z",
                    source_rev(source, "base")?,
                    source_rev(source, "head")?,
                ])
            } else {
                tree_args(source_rev(source, "head")?)?
            };
            names(source, &cmd, |path| {
                if !path.starts_with(prefix) {
                    return Ok(());
                }
                let index = total;
                total += 1;
                if index < offset || filled {
                    return Ok(());
                }
                let len = path.chars().count();
                if files.len() >= count || chars + len > 200_000 {
                    filled = true;
                    return Ok(());
                }
                chars += len;
                files.push(path.to_owned());
                Ok(())
            })
            .await?;
            let mut result = page(total, offset, files.len());
            result["files"] = json!(files);
            Ok(result)
        }
        "read_file" => {
            let path = path_arg(&args["path"])?;
            ensure!(
                args.get("revision")
                    .is_none_or(|v| v == "head" || v == "base"),
                "revision must be head or base"
            );
            for key in ["start", "count"] {
                ensure!(
                    args.get(key).is_none_or(Value::is_number),
                    "Line start and count must be numbers"
                );
            }
            let start = args["start"]
                .as_f64()
                .filter(|v| *v != 0.0)
                .unwrap_or(1.0)
                .trunc()
                .max(1.0) as usize;
            let count = args["count"]
                .as_f64()
                .filter(|v| *v != 0.0)
                .unwrap_or(200.0)
                .trunc()
                .clamp(1.0, 500.0) as usize;
            let rev = if args["revision"] == "base" {
                source_rev(source, "base")?
            } else {
                source_rev(source, "head")?
            };
            let text = read_blob(source, path, Some(rev)).await?;
            Ok(text
                .split('\n')
                .skip(start - 1)
                .take(count)
                .enumerate()
                .map(|(i, line)| format!("{}: {line}", start + i))
                .collect::<Vec<_>>()
                .join("\n")
                .into())
        }
        "diff" => {
            let (offset, count) = bounds(args, 200_000)?;
            let path = args.get("path").map(path_arg).transpose()?;
            diff(source, path, offset, count, CancellationToken::new()).await
        }
        "search" => {
            let text = args["text"]
                .as_str()
                .filter(|s| !s.is_empty() && s.chars().count() <= 1000)
                .ok_or_else(|| anyhow!("Search needs 1–1000 literal characters"))?;
            let mut cmd = strings(&[
                "grep",
                "-n",
                "-I",
                "-F",
                "-e",
                text,
                source_rev(source, "head")?,
                "--",
            ]);
            if let Some(path) = args.get("path") {
                cmd.push(path_arg(path)?.to_owned());
            }
            match git_bytes(
                source_dir(source)?,
                &cmd,
                BLOB_LIMIT,
                &BTreeMap::new(),
                CancellationToken::new(),
            )
            .await
            {
                Ok(bytes) => Ok(String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(100_000)
                    .collect::<String>()
                    .into()),
                Err(e) if e.downcast_ref::<GitFailure>().is_some_and(|e| e.code == 1) => {
                    Ok("".into())
                }
                Err(e) => Err(e),
            }
        }
        _ => bail!("Unknown inspection tool"),
    }
}

pub fn tools() -> Value {
    json!([
        {"name":"list_files","description":"List tracked files or every changed path including deletions. Follow nextOffset until null before treating the list as complete.","inputSchema":{"type":"object","properties":{"prefix":{"type":"string"},"changed_only":{"type":"boolean"},"offset":{"type":"integer","minimum":0},"count":{"type":"integer","minimum":1,"maximum":10000}},"required":[],"additionalProperties":false}},
        {"name":"read_file","description":"Read numbered lines of a regular tracked file. Symlinks are never followed.","inputSchema":{"type":"object","properties":{"path":{"type":"string"},"revision":{"enum":["head","base"]},"start":{"type":"integer"},"count":{"type":"integer"}},"required":["path"],"additionalProperties":false}},
        {"name":"diff","description":"Read the PR diff against its merge base. offset/count count Unicode characters. Follow nextOffset until null to read the complete comparison. Discover paths independently with list_files changed_only=true.","inputSchema":{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":0},"count":{"type":"integer","minimum":1,"maximum":200000}},"required":[],"additionalProperties":false}},
        {"name":"search","description":"Search tracked source using a literal string. Does not execute repository code.","inputSchema":{"type":"object","properties":{"text":{"type":"string"},"path":{"type":"string"}},"required":["text"],"additionalProperties":false}}
    ])
}

// Runtime calls recover orphan containers before doing their named operation.
// Delegation launches reserve state before awaited writes. Both must finish
// cancellation/transactions before their futures are dropped.
fn tool_needs_drain(name: &str) -> bool {
    crate::execution::TOOL_NAMES.contains(&name)
        || matches!(
            name,
            "start_review_task" | "resume_review_task" | "restart_review_task"
        )
}
fn tool_starts_work(name: &str) -> bool {
    matches!(
        name,
        "run_experiment"
            | "prepare_environment"
            | "start_review_task"
            | "resume_review_task"
            | "restart_review_task"
    )
}

pub(crate) async fn cancelled_tool(
    name: &str,
    call: impl std::future::Future<Output = Result<Value>>,
) -> Result<Value> {
    if tool_needs_drain(name) {
        call.await
    } else {
        Err(anyhow!("Tool request cancelled"))
    }
}

// Retain partial writes across select! cancellation. Backpressure may delay
// delivery, but must not stop tool deadlines or input cancellation processing.
#[derive(Default)]
struct McpOutbox {
    messages: VecDeque<Vec<u8>>,
    offset: usize,
    bytes: usize,
}
impl McpOutbox {
    fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
    fn push(&mut self, response: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(response)?;
        bytes.push(b'\n');
        ensure!(
            self.messages.len() < 64 && self.bytes + bytes.len() <= 16 * 1024 * 1024,
            "MCP output backlog exceeded; client is not consuming responses"
        );
        self.bytes += bytes.len();
        self.messages.push_back(bytes);
        Ok(())
    }
    async fn write_next<W: tokio::io::AsyncWrite + Unpin>(&mut self, stdout: &mut W) -> Result<()> {
        let bytes = self.messages.front().context("Empty MCP output queue")?;
        let count = stdout.write(&bytes[self.offset..]).await?;
        ensure!(count > 0, "MCP output closed");
        self.offset += count;
        if self.offset == bytes.len() {
            self.bytes -= bytes.len();
            self.messages.pop_front();
            self.offset = 0;
        }
        Ok(())
    }
}

// The buffer survives cancellation of this future when an active tool finishes.
// fill_buf and consume keep partial JSON lines intact across select! iterations.
async fn mcp_request<R: tokio::io::AsyncBufRead + Unpin>(
    stdin: &mut R,
    line: &mut Vec<u8>,
) -> Result<Option<Value>> {
    loop {
        let chunk = stdin.fill_buf().await?;
        if chunk.is_empty() && line.is_empty() {
            return Ok(None);
        }
        let eof = chunk.is_empty();
        let n = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |n| n + 1);
        ensure!(
            line.len() + n <= 4 * 1024 * 1024,
            "MCP request exceeds 4 MiB"
        );
        let complete = eof || chunk[n - 1] == b'\n';
        line.extend_from_slice(&chunk[..n]);
        stdin.consume(n);
        if complete {
            let request = serde_json::from_slice::<Value>(line);
            line.clear();
            if let Ok(request) = request
                && request.is_object()
            {
                return Ok(Some(request));
            }
        }
    }
}

pub async fn inspection_main(source_path: &Path, context_path: Option<&Path>) -> Result<()> {
    let source: Value = serde_json::from_slice(&tokio::fs::read(source_path).await?)?;
    let context: Option<Value> = if let Some(path) = context_path {
        Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    } else {
        None
    };
    let execution = context
        .as_ref()
        .map(|context| {
            crate::execution::Execution::from_context(
                context,
                &source_path
                    .parent()
                    .context("Missing review directory")?
                    .join("experiments"),
            )
        })
        .transpose()?
        .flatten();
    let delegation = if let Some(context) = context.filter(|v| {
        v["job"]["settings"]["subagents"]["max"]
            .as_u64()
            .unwrap_or(0)
            > 0
    }) {
        Some(crate::delegation::Delegation::init(context).await?)
    } else {
        None
    };
    let mut definitions = tools().as_array().expect("tool array").clone();
    if delegation.is_some() {
        for mut tool in crate::delegation::tools()
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
        {
            if tool.get("inputSchema").is_none() {
                let props = tool
                    .as_object_mut()
                    .unwrap()
                    .remove("properties")
                    .unwrap_or(json!({}));
                let required = tool
                    .as_object_mut()
                    .unwrap()
                    .remove("required")
                    .unwrap_or(json!([]));
                tool["inputSchema"] = json!({"type":"object","properties":props,"required":required,"additionalProperties":false});
            }
            definitions.push(tool);
        }
    }
    if execution.is_some() {
        definitions.extend(crate::execution::tools());
    }
    let mut stdin = BufReader::new(crate::process::NonblockingIo::stdin()?);
    let mut stdout = crate::process::NonblockingIo::stdout()?;
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let stop = async {
        #[cfg(unix)]
        {
            tokio::select! {_ = tokio::signal::ctrl_c()=>{}, _=term.recv()=>{}}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::pin!(stop);
    let result=async {
        let mut line = Vec::new();
        let mut pending = VecDeque::new();
        let mut input_closed = false;
        let mut outbox = McpOutbox::default();
        'requests: loop {
            let request = loop {
                if let Some(request) = pending.pop_front() { break request; }
                if input_closed && outbox.is_empty() { break 'requests; }
                tokio::select! {
                    _ = &mut stop => break 'requests,
                    written = outbox.write_next(&mut stdout), if !outbox.is_empty() => written?,
                    request = mcp_request(&mut stdin, &mut line), if !input_closed => match request? {
                        Some(request) => break request,
                        None => input_closed = true,
                    }
                }
            };
            if request.get("id").is_none(){continue;}
            let id=request["id"].clone();
            let response=match request["method"].as_str(){
                Some("initialize")=>json!({"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"crow-inspection","version":"1.0.0"}}}),
                Some("tools/list")=>json!({"result":{"tools":definitions}}), Some("ping")=>json!({"result":{}}),
                Some("tools/call")=>{
                    let cancel = CancellationToken::new();
                    let call=async {
                        let params=&request["params"]; let name=params["name"].as_str().ok_or_else(||anyhow!("Invalid tool request"))?;
                        let empty=json!({}); let args=params.get("arguments").filter(|v|!v.is_null()).unwrap_or(&empty);
                        if let Some(delegate)=&delegation&& crate::delegation::tools().as_array().is_some_and(|ts|ts.iter().any(|t|t["name"]==name)){return delegate.call(name,args).await;}
                        if let Some(execution) = &execution && crate::execution::TOOL_NAMES.contains(&name) { return execution.call(name, args, cancel.clone()).await; }
                        inspection_tool(&source,name,args).await
                    };
                    let name = request["params"]["name"].as_str().unwrap_or("");
                    let experiment = execution.is_some() && crate::execution::TOOL_NAMES.contains(&name);
                    let mutating = tool_starts_work(name);
                    // A preceding read-only call may have drained stdin to EOF
                    // while this state-changing request was waiting in the queue.
                    if input_closed && mutating { continue 'requests; }
                    tokio::pin!(call);
                    let output = loop {
                        tokio::select! {
                            _ = &mut stop => {
                                cancel.cancel();
                                let _ = cancelled_tool(name, &mut call).await;
                                break 'requests;
                            },
                            result = &mut call => break result,
                            written = outbox.write_next(&mut stdout), if !outbox.is_empty() => {
                                if let Err(error) = written {
                                    cancel.cancel();
                                    let _ = cancelled_tool(name, &mut call).await;
                                    return Err(error);
                                }
                            },
                            incoming = mcp_request(&mut stdin, &mut line), if !input_closed => {
                                let incoming = match incoming {
                                    Ok(Some(incoming)) => incoming,
                                    Ok(None) if !mutating => {
                                        input_closed = true;
                                        continue;
                                    },
                                    closed => {
                                        cancel.cancel();
                                        let _ = cancelled_tool(name, &mut call).await;
                                        closed?;
                                        break 'requests;
                                    }
                                };
                                if incoming.get("id").is_none() {
                                    if incoming["method"] == "notifications/cancelled"
                                        && let Some(cancelled_id) = incoming["params"].get("requestId") {
                                        if cancelled_id == &id {
                                            cancel.cancel();
                                            break cancelled_tool(name, &mut call).await;
                                        } else {
                                            pending.retain(|request: &Value| request.get("id") != Some(cancelled_id));
                                        }
                                    }
                                } else {
                                    // Keep calls serial while still accepting cancellation.
                                    // Limit queued requests as well as each input line.
                                    if pending.len() == 16 {
                                        let busy = json!({"jsonrpc":"2.0","id":incoming["id"],"error":{"code":-32000,"message":"MCP request queue is full; retry after pending requests complete"}});
                                        if let Err(error) = outbox.push(&busy) {
                                            cancel.cancel();
                                            let _ = cancelled_tool(name, &mut call).await;
                                            return Err(error);
                                        }
                                    } else {
                                        pending.push_back(incoming);
                                    }
                                }
                            }
                        }
                    };
                    match output {Ok(value) if experiment && request["params"]["name"] == "read_artifact" => json!({"result":value}),Ok(value)=>json!({"result":{"content":[{"type":"text","text":value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())}]}}),Err(e)=>json!({"result":{"isError":true,"content":[{"type":"text","text":e.to_string()}]}})}
                }
                _=>json!({"error":{"code":-32601,"message":"Unsupported method"}}),
            };
            let mut response=response; response["jsonrpc"]="2.0".into(); response["id"]=id;
            outbox.push(&response)?;

        } Ok::<_,anyhow::Error>(())
    }.await;
    if let Some(delegation) = delegation {
        delegation.close().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mcp_output_backlog_bounds_count_and_bytes() {
        let mut outbox = McpOutbox::default();
        for _ in 0..64 {
            outbox.push(&json!({"id":1,"result":{}})).unwrap();
        }
        assert!(outbox.push(&json!({"id":2,"result":{}})).is_err());
        let mut outbox = McpOutbox::default();
        let large = json!({"id":"x".repeat(8 * 1024 * 1024)});
        outbox.push(&large).unwrap();
        assert!(outbox.push(&large).is_err());
    }

    #[test]
    fn cancellation_policy_covers_every_advertised_tool() {
        let mut advertised: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .chain(crate::delegation::tools().as_array().unwrap().iter())
            .chain(crate::execution::tools().iter())
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        advertised.sort();
        let mut expected = vec![
            ("list_files", false, false),
            ("read_file", false, false),
            ("diff", false, false),
            ("search", false, false),
            ("review_task_status", false, false),
            ("wait_review_task", false, false),
            ("discover_environment", true, false),
            ("list_experiments", true, false),
            ("read_artifact", true, false),
            ("prepare_environment", true, true),
            ("run_experiment", true, true),
            ("start_review_task", true, true),
            ("resume_review_task", true, true),
            ("restart_review_task", true, true),
        ];
        expected.sort_by_key(|(name, _, _)| *name);
        assert_eq!(
            advertised,
            expected
                .iter()
                .map(|(name, _, _)| name.to_string())
                .collect::<Vec<_>>()
        );
        for (name, drain, mutating) in expected {
            assert_eq!(tool_needs_drain(name), drain, "{name}");
            assert_eq!(tool_starts_work(name), mutating, "{name}");
        }
    }

    #[tokio::test]
    async fn cancelled_read_only_task_waits_do_not_block_shutdown() {
        for name in [
            "list_files",
            "read_file",
            "diff",
            "search",
            "review_task_status",
            "wait_review_task",
        ] {
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                cancelled_tool(name, std::future::pending::<Result<Value>>()),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        }
    }

    async fn fixture(huge: bool) -> (TempDir, Value) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        async fn run(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env_clear()
                .envs(crate::util::clean_env())
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
        run(root, &["init", "-b", "main"]).await;
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "Trusted root rules.").unwrap();
        std::fs::write(root.join("src/api.js"), "const old = 1;\n").unwrap();
        std::fs::write(root.join("deleted.txt"), "removed\n").unwrap();
        std::fs::write(root.join("binary.dat"), [0, 1, 2]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", root.join("outside")).unwrap();
        run(root, &["add", "."]).await;
        run(root, &["commit", "-m", "base"]).await;
        let base = run(root, &["rev-parse", "HEAD"]).await;
        std::fs::write(root.join("AGENTS.md"), "Untrusted changed rules.").unwrap();
        std::fs::write(
            root.join("src/api.js"),
            "const old = 1;\nconst changed = 2;\n",
        )
        .unwrap();
        std::fs::write(root.join("$(touch SHOULD_NOT_EXIST).txt"), "literal-name").unwrap();
        std::fs::write(root.join("tabs\t\"quote.txt"), "odd name").unwrap();
        std::fs::write(root.join("[glob].txt"), "literal glob").unwrap();
        std::fs::write(root.join("g.txt"), "must not match glob path").unwrap();
        std::fs::remove_file(root.join("deleted.txt")).unwrap();
        if huge {
            std::fs::write(
                root.join("a-large.txt"),
                format!("{}\n", "x".repeat(17 * 1024 * 1024)),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("z-later.txt"),
            "late change must remain reachable\n",
        )
        .unwrap();
        run(root, &["add", "."]).await;
        run(root, &["commit", "-m", "head"]).await;
        let head = run(root, &["rev-parse", "HEAD"]).await;
        let bare = root.join("bare.git");
        run(
            root,
            &[
                "clone",
                "--bare",
                root.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        )
        .await;
        (
            dir,
            json!({"dir":bare,"base":base,"head":head,"targetSha":base,"target":"main"}),
        )
    }

    #[tokio::test]
    async fn execution_archive_obeys_storage_budget() {
        let (dir, source) = fixture(false).await;
        let output = dir.path().join("source.tar");
        let error = execution_archive(&source, "head", &output, 1024, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 1024 bytes"));
        assert!(std::fs::metadata(&output).unwrap().len() <= 1024);
        execution_archive(
            &source,
            "head",
            &output,
            1024 * 1024,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(std::fs::metadata(&output).unwrap().len() > 1024);
    }

    #[test]
    fn rejects_revision_and_path_injection() {
        assert!(revision(&"a".repeat(40)).is_ok());
        for bad in ["HEAD", "--help", "a;echo x", &"a".repeat(39)] {
            assert!(revision(bad).is_err());
        }
        for bad in [
            "",
            "/etc/passwd",
            "../secret",
            "x/../../secret",
            "x\\secret",
            "x\0y",
            "x\ny",
            "a//b",
        ] {
            assert!(!safe_path(bad));
        }
        assert!(safe_path("$(touch file).txt"));
        assert!(safe_path("tabs\t\"quote.txt"));
    }
    #[tokio::test]
    async fn reads_only_pinned_regular_blobs_and_literal_paths() {
        let (dir, source) = fixture(false).await;
        assert!(
            read_blob(&source, "src/api.js", None)
                .await
                .unwrap()
                .contains("changed")
        );
        assert_eq!(
            read_blob(
                &source,
                "src/api.js",
                Some(source["base"].as_str().unwrap())
            )
            .await
            .unwrap(),
            "const old = 1;\n"
        );
        for path in ["outside", "binary.dat", "missing", "src"] {
            assert!(read_blob(&source, path, None).await.is_err(), "{path}");
        }
        assert_eq!(
            read_blob(&source, "tabs\t\"quote.txt", None).await.unwrap(),
            "odd name"
        );
        assert_eq!(
            read_blob(&source, "[glob].txt", None).await.unwrap(),
            "literal glob"
        );
        assert_eq!(
            read_blob(&source, "$(touch SHOULD_NOT_EXIST).txt", None)
                .await
                .unwrap(),
            "literal-name"
        );
        assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
    }
    #[tokio::test]
    async fn guidance_is_target_pinned_and_fingerprint_compatible() {
        let (_dir, source) = fixture(false).await;
        let rules = guidance(&source).await.unwrap();
        assert_eq!(
            rules["files"],
            json!([{"path":"AGENTS.md","body":"Trusted root rules."}])
        );
        let expected = hex::encode(Sha256::digest(
            br#"[{"path":"AGENTS.md","body":"Trusted root rules."}]"#,
        ));
        assert_eq!(rules["fingerprint"], expected);
    }
    #[tokio::test]
    async fn tool_bounds_and_literal_search() {
        let (_dir, source) = fixture(false).await;
        assert_eq!(
            inspection_tool(
                &source,
                "read_file",
                &json!({"path":"src/api.js","start":2,"count":1})
            )
            .await
            .unwrap(),
            "2: const changed = 2;"
        );
        assert_eq!(
            inspection_tool(
                &source,
                "search",
                &json!({"text":"must not match","path":"[glob].txt"})
            )
            .await
            .unwrap(),
            ""
        );
        assert!(
            inspection_tool(
                &source,
                "search",
                &json!({"text":"changed","path":"src/api.js"})
            )
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .contains("const changed")
        );
        for name in ["list_files", "diff"] {
            for args in [
                json!({"offset":-1}),
                json!({"offset":0.5}),
                json!({"offset":"0"}),
                json!({"count":0}),
                json!({"count":200001}),
            ] {
                assert!(inspection_tool(&source, name, &args).await.is_err());
            }
            let exhausted = inspection_tool(&source, name, &json!({"offset":1000000}))
                .await
                .unwrap();
            assert!(exhausted["nextOffset"].is_null());
            assert_eq!(exhausted["truncated"], false);
        }
        assert!(
            inspection_tool(&source, "shell", &json!({"command":"touch x"}))
                .await
                .is_err()
        );
        assert!(
            inspection_tool(&source, "list_files", &json!([]))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn large_diff_streams_and_publication_keeps_only_added_anchors() {
        let (_dir, source) = fixture(true).await;
        let first = diff(&source, None, 0, 200_000, CancellationToken::new())
            .await
            .unwrap();
        assert!(first["total"].as_u64().unwrap() > 17 * 1024 * 1024);
        assert_eq!(first["patch"].as_str().unwrap().len(), 200_000);
        assert_eq!(first["nextOffset"], 200_000);
        let last = diff(
            &source,
            None,
            first["total"].as_u64().unwrap() as usize - 2000,
            200_000,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            last["patch"]
                .as_str()
                .unwrap()
                .contains("late change must remain reachable")
        );
        assert!(last["nextOffset"].is_null());
        let patch=publication_patch(&source,&json!([{"path":"a-large.txt","line":1},{"path":"z-later.txt","line":1},{"path":"src/api.js","line":1},{"path":"src/api.js","line":2},{"path":"deleted.txt","line":1}]),CancellationToken::new()).await.unwrap();
        assert!(patch.len() < 1000);
        assert!(patch.contains("+++ b/a-large.txt\n@@ -0,0 +1,1 @@"));
        assert!(patch.contains("+++ b/src/api.js\n@@ -0,0 +2,1 @@"));
        assert!(!patch.contains("deleted.txt"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(diff(&source, None, 0, 20, cancel).await.is_err());
        let changed = inspection_tool(&source, "list_files", &json!({"changed_only":true}))
            .await
            .unwrap();
        assert!(
            changed["files"]
                .as_array()
                .unwrap()
                .contains(&json!("deleted.txt"))
        );
    }
    #[tokio::test]
    async fn git_ignores_external_diff_and_text_conversion() {
        let (dir, source) = fixture(false).await;
        let canary = dir.path().join("external-ran");
        let script = dir.path().join("external.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", canary.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        git(
            source_dir(&source).unwrap(),
            &strings(&["config", "diff.external", script.to_str().unwrap()]),
        )
        .await
        .unwrap();
        let patch = diff(&source, None, 0, 200000, CancellationToken::new())
            .await
            .unwrap();
        assert!(patch["patch"].as_str().unwrap().contains("const changed"));
        assert!(!canary.exists());
    }
    #[tokio::test]
    async fn huge_tree_streams_without_an_aggregate_capture_limit() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &strings(&["init", "--bare"]))
            .await
            .unwrap();
        async fn input(dir: &Path, args: &[&str], input: String) -> String {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env_clear()
                .envs(crate::util::clean_env())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .await
                .unwrap();
            let output = child.wait_with_output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        let empty = input(dir.path(), &["hash-object", "-w", "--stdin"], String::new()).await;
        let trusted = input(
            dir.path(),
            &["hash-object", "-w", "--stdin"],
            "Trusted rules".into(),
        )
        .await;
        let leaves: Vec<_> = (0..1000)
            .map(|i| format!("f{i:04}-{}.txt", "x".repeat(180)))
            .collect();
        let subtree = input(
            dir.path(),
            &["mktree", "-z"],
            leaves
                .iter()
                .map(|name| format!("100644 blob {empty}\t{name}\0"))
                .collect(),
        )
        .await;
        let mut entries = (0..100)
            .map(|i| format!("040000 tree {subtree}\tdir{i:03}\0"))
            .collect::<String>();
        entries.push_str(&format!("100644 blob {trusted}\tAGENTS.md\0"));
        entries.push_str(&format!("100644 blob {empty}\tz-later.txt\0"));
        let head = input(dir.path(), &["mktree", "-z"], entries).await;
        let base = input(
            dir.path(),
            &["mktree", "-z"],
            format!("100644 blob {empty}\tdeleted.txt\0"),
        )
        .await;
        let source = json!({"dir":dir.path(),"head":head,"base":base,"targetSha":head});
        let first = inspection_tool(&source, "list_files", &json!({}))
            .await
            .unwrap();
        assert_eq!(first["total"], 100002);
        assert_eq!(
            first["nextOffset"],
            first["files"].as_array().unwrap().len()
        );
        assert!(
            first["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().len())
                .sum::<usize>()
                <= 200000
        );
        let last = inspection_tool(&source, "list_files", &json!({"offset":100000}))
            .await
            .unwrap();
        assert_eq!(
            last["files"],
            json!([format!("dir099/{}", leaves[999]), "z-later.txt"])
        );
        let changed = inspection_tool(
            &source,
            "list_files",
            &json!({"changed_only":true,"offset":100001}),
        )
        .await
        .unwrap();
        assert_eq!(changed["total"], 100003);
        assert_eq!(changed["files"], last["files"]);
        assert_eq!(
            guidance(&source).await.unwrap()["files"],
            json!([{"path":"AGENTS.md","body":"Trusted rules"}])
        );
    }

    #[test]
    fn utf8_stream_preserves_split_codepoints() {
        let mut stream = TextStream::default();
        let mut text = String::new();
        for byte in "a😀éz".as_bytes() {
            stream.feed(&[*byte], |s| text.push_str(s));
        }
        stream.finish(|s| text.push_str(s));
        assert_eq!(text, "a😀éz");
    }
}
