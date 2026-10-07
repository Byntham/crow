//! Crow's managed runtime image. The worker builds it in the background, so a
//! review never waits for a toolchain build inside a tool call.
use super::podman;
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, time::Duration};

const CONTAINERFILE: &str = include_str!("Containerfile");
const PROXY: &str = include_str!("proxy.py");
/// Toolchains in the managed image, reported to the reviewer.
pub(super) const TOOLCHAINS: &[&str] = &[
    "bash", "python3", "pip", "node", "npm", "cargo", "rustc", "go", "gcc", "make", "git",
    "sqlite3",
];

/// The image tag changes whenever its build inputs change.
pub fn tag() -> String {
    let digest = crate::util::hash_bytes(format!("{CONTAINERFILE}\0{PROXY}").as_bytes());
    format!("localhost/crow-runtime:{}", &digest[..16])
}

pub async fn exists(executable: &str, env: &BTreeMap<String, String>) -> Result<bool> {
    let output = podman(executable, env, &["image", "exists", &tag()], false).await?;
    Ok(output.success)
}

/// Build the current image, then remove older Crow-managed images.
pub async fn build(executable: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let context = tempfile::tempdir()?;
    std::fs::write(context.path().join("Containerfile"), CONTAINERFILE)?;
    std::fs::write(context.path().join("proxy.py"), PROXY)?;
    let tag = tag();
    let path = context.path().to_string_lossy().into_owned();
    let built = crate::process::run(
        executable,
        &[
            "build".into(),
            "--pull=missing".into(),
            "--label=crow.managed=true".into(),
            format!("--tag={tag}"),
            path,
        ],
        crate::process::RunOptions {
            env: Some(env.clone()),
            timeout: Some(Duration::from_secs(1800)),
            ..Default::default()
        },
    )
    .await;
    built.context("Could not build Crow's runtime image")?;
    let listed = podman(
        executable,
        env,
        &[
            "images",
            "--filter=label=crow.managed=true",
            "--format={{.Repository}}:{{.Tag}}",
        ],
        true,
    )
    .await?;
    for old in listed.stdout.lines().map(str::trim) {
        if !old.is_empty() && old != tag && old.starts_with("localhost/crow-runtime:") {
            let removed = podman(executable, env, &["rmi", old], false).await?;
            ensure!(
                removed.success,
                "Could not remove old runtime image {old}: {}",
                removed.stderr.trim()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tag_tracks_build_inputs() {
        let tag = super::tag();
        assert!(tag.starts_with("localhost/crow-runtime:"));
        assert_eq!(tag.len(), "localhost/crow-runtime:".len() + 16);
        assert_eq!(tag, super::tag());
    }
}
