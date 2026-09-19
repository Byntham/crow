//! Trusted runtime stages. Public status messages never include command output or host errors.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    RuntimeCheck,
    ImageProvision,
    CacheRestore,
    SourceArchive,
    GatewayStart,
    ContainerStart,
    WorkspaceRestore,
    SourceRestore,
    SetupCommand,
    TestCommand,
    SnapshotExport,
    ArtifactCollection,
    CacheSave,
    Cleanup,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::RuntimeCheck => "runtime availability check",
            Self::ImageProvision => "toolchain image preparation",
            Self::CacheRestore => "dependency cache restore",
            Self::SourceArchive => "source archive creation",
            Self::GatewayStart => "package gateway startup",
            Self::ContainerStart => "container startup",
            Self::WorkspaceRestore => "workspace restore and resource limits",
            Self::SourceRestore => "pinned source restore",
            Self::SetupCommand => "dependency setup command",
            Self::TestCommand => "test command",
            Self::SnapshotExport => "prepared environment export",
            Self::ArtifactCollection => "screenshot collection",
            Self::CacheSave => "dependency cache save",
            Self::Cleanup => "container cleanup",
        }
    }
}

pub struct Trace {
    path: PathBuf,
    stage: std::sync::Mutex<Stage>,
    container_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Trace {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            stage: std::sync::Mutex::new(Stage::RuntimeCheck),
            container_started: Default::default(),
        }
    }

    pub fn set(&self, stage: Stage) -> Result<()> {
        *self.stage.lock().unwrap() = stage;
        let mut record = crate::util::read_json(&self.path)?.unwrap_or_default();
        record["stage"] = json!(stage);
        if stage == Stage::ContainerStart {
            record["containerStarted"] = json!(true);
        }
        crate::util::atomic(&self.path, &record)?;
        if stage == Stage::ContainerStart {
            self.container_started
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn container_started(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.container_started.clone()
    }

    pub fn started(&self) -> bool {
        self.container_started
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn finish(&self, record: &mut Value) {
        let stage = *self.stage.lock().unwrap();
        record["stage"] = json!(stage);
        record["containerStarted"] = json!(self.started());
        if record["status"] != "passed" && record.get("failureStage").is_none() {
            record["failureStage"] = json!(stage);
        }
    }
}

/// Keep diagnostic metadata bounded even if a runtime returns a very large error.
pub fn bounded_error(error: impl std::fmt::Display) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    let mut bounded: String = chars.by_ref().take(4096).collect();
    if chars.next().is_some() {
        bounded.push_str(" [truncated]");
    }
    bounded
}

pub fn warnings(record: &Value) -> BTreeMap<Stage, u32> {
    let mut warnings = BTreeMap::new();
    if record.get("cleanupError").is_some() && record.get("cleanupRecoveredAt").is_none() {
        warnings.insert(Stage::Cleanup, 1);
    }
    for (field, stage) in [
        ("cacheRestoreError", Stage::CacheRestore),
        ("cacheSaveError", Stage::CacheSave),
    ] {
        if record.get(field).is_some() {
            warnings.insert(stage, 1);
        }
    }
    if let Some(artifacts) = record["artifacts"].as_array() {
        for artifact in artifacts.iter().take(3) {
            if artifact["saved"] == false {
                *warnings.entry(Stage::ArtifactCollection).or_default() += 1;
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_persists_the_last_stage_for_crash_and_failure_diagnosis() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipt.json");
        crate::util::atomic(&path, &json!({"status":"running","phase":"test"})).unwrap();
        let trace = Trace::new(&path);
        trace.set(Stage::ContainerStart).unwrap();
        let mut receipt = crate::util::read_json(&path).unwrap().unwrap();
        assert_eq!(receipt["stage"], "container_start");
        receipt["status"] = json!("timed_out");
        trace.finish(&mut receipt);
        assert_eq!(receipt["failureStage"], "container_start");
        assert!(serde_json::from_value::<Stage>(json!("secret host error")).is_err());
    }

    #[test]
    fn error_details_are_bounded_without_breaking_utf8() {
        assert_eq!(bounded_error("short"), "short");
        let long = bounded_error("é".repeat(5000));
        assert!(long.ends_with(" [truncated]"));
        assert_eq!(long.chars().count(), 4108);
    }
}
