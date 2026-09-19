//! Small, command-free runtime summaries sent by the worker to the service.
use crate::runtime_diagnostics::Stage;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Counts {
    pub passed: u32,
    pub failed: u32,
    pub blocked: u32,
    pub timed_out: u32,
    pub interrupted: u32,
    pub running: u32,
}
impl Counts {
    fn total(&self) -> u64 {
        [
            self.passed,
            self.failed,
            self.blocked,
            self.timed_out,
            self.interrupted,
            self.running,
        ]
        .into_iter()
        .map(u64::from)
        .sum()
    }
    fn add(&mut self, status: &str) {
        match status {
            "passed" => self.passed += 1,
            "failed" => self.failed += 1,
            "timed_out" => self.timed_out += 1,
            "interrupted" => self.interrupted += 1,
            "running" => self.running += 1,
            _ => self.blocked += 1,
        }
    }
    fn summary(&self, active: bool) -> String {
        let mut parts = Vec::new();
        for (count, label) in [
            (self.passed, "passed"),
            (self.failed, "failed"),
            (self.blocked, "blocked"),
            (self.timed_out, "timed out"),
            (
                self.interrupted + if active { 0 } else { self.running },
                "interrupted",
            ),
            (if active { self.running } else { 0 }, "running"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        parts.join(", ")
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub enabled: bool,
    pub setup: Counts,
    pub tests: Counts,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "activeStage"
    )]
    pub active_stage: Option<Stage>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub failures: BTreeMap<Stage, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub warnings: BTreeMap<Stage, u32>,
}
impl Progress {
    pub fn validate(value: &Value) -> Result<Self> {
        let progress: Self = serde_json::from_value(value.clone())?;
        let total = progress.setup.total() + progress.tests.total();
        ensure!(
            total <= 50 && (progress.enabled || total == 0),
            "Invalid runtime counters"
        );
        ensure!(
            progress
                .failures
                .values()
                .map(|n| u64::from(*n))
                .sum::<u64>()
                <= total
                && progress
                    .warnings
                    .values()
                    .map(|n| u64::from(*n))
                    .sum::<u64>()
                    <= total * 6
                && (progress.active_stage.is_none()
                    || progress.setup.running + progress.tests.running > 0)
                && progress.warnings.keys().all(|stage| matches!(
                    stage,
                    Stage::Cleanup
                        | Stage::ArtifactCollection
                        | Stage::CacheRestore
                        | Stage::CacheSave
                )),
            "Invalid runtime diagnostics"
        );
        Ok(progress)
    }
    pub fn read(root: &Path, job: &Value) -> Result<Self> {
        let mut progress = Self {
            enabled: crate::execution::enabled(
                &job["settings"],
                job["repo"].as_str().unwrap_or(""),
            )?,
            ..Default::default()
        };
        if !progress.enabled {
            return Ok(progress);
        }
        let dir = root
            .join("reviews")
            .join(job["id"].as_str().unwrap_or(""))
            .join("experiments");
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(progress),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let record = crate::util::read_json(&path)?.unwrap_or_default();
            let counts = match record["phase"].as_str() {
                Some("setup") => &mut progress.setup,
                Some("test") => &mut progress.tests,
                _ => continue,
            };
            counts.add(record["status"].as_str().unwrap_or("error"));
            if record["status"] == "running" {
                progress.active_stage = serde_json::from_value(record["stage"].clone()).ok();
            } else if record["status"] != "passed"
                && let Ok(stage) = serde_json::from_value(record["failureStage"].clone())
            {
                *progress.failures.entry(stage).or_default() += 1;
            }
            for (stage, count) in crate::runtime_diagnostics::warnings(&record) {
                *progress.warnings.entry(stage).or_default() += count;
            }
        }
        Self::validate(&serde_json::to_value(progress)?)
    }
    pub fn render(&self, active: bool) -> String {
        if !self.enabled {
            return "Disabled by worker configuration.".into();
        }
        let label = if active && self.setup.running > 0 {
            "Setting up the test environment"
        } else if active && self.tests.running > 0 {
            "Running tests"
        } else if self.tests.total() > 0 {
            if active { "Investigating" } else { "Finished" }
        } else if self.setup.passed == 0 && self.setup.total() > 0 {
            "Testing blocked or interrupted during setup"
        } else if active {
            "Reviewer is deciding what to test"
        } else {
            "Not attempted by the reviewer"
        };
        let mut text = format!("{label}.");
        if self.tests.total() > 0 {
            text.push_str(&format!(" Test commands: {}.", self.tests.summary(active)));
        }
        if self.setup.total() > 0 {
            text.push_str(&format!(" Setup attempts: {}.", self.setup.summary(active)));
        }
        if self.tests.failed > 0 {
            text.push_str(" Failed commands can include base failures and investigation attempts; see the review for confirmed bugs.");
        }
        if active && let Some(stage) = self.active_stage {
            text.push_str(&format!(" Current step: {}.", stage.label()));
        }
        for (label, stages) in [
            ("Failure locations", &self.failures),
            ("Warnings", &self.warnings),
        ] {
            if !stages.is_empty() {
                let parts = stages
                    .iter()
                    .map(|(stage, count)| format!("{count} at {}", stage.label()))
                    .collect::<Vec<_>>();
                text.push_str(&format!(" {label}: {}.", parts.join(", ")));
            }
        }
        if !self.failures.is_empty() || !self.warnings.is_empty() {
            text.push_str(" Worker receipts contain the error details.");
        }
        text
    }
}

pub fn render(value: &Value, state: &str) -> String {
    match Progress::validate(value) {
        Ok(progress) => {
            let text = progress.render(state == "reviewing");
            if !matches!(state, "reviewing" | "publishing" | "completed") {
                text.replacen("Finished.", "Stopped.", 1).replacen(
                    "Not attempted by the reviewer.",
                    "No test commands recorded.",
                    1,
                )
            } else {
                text
            }
        }
        Err(_) => {
            if matches!(state, "queued" | "reviewing") {
                "Waiting for the worker's runtime status.".into()
            } else {
                "Runtime status was not reported by the worker.".into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_status_distinguishes_setup_tests_and_no_attempt() {
        let mut p = Progress {
            enabled: true,
            ..Default::default()
        };
        assert!(p.render(true).contains("deciding"));
        assert!(p.render(false).contains("Not attempted"));
        p.setup.running = 1;
        assert!(p.render(true).contains("Setting up"));
        assert!(p.render(false).contains("1 interrupted"));
        p.setup.running = 0;
        p.setup.failed = 1;
        assert!(
            p.render(false)
                .contains("blocked or interrupted during setup")
        );
        p.setup.passed = 1;
        p.tests.running = 1;
        assert!(p.render(true).contains("Running tests"));
        p.tests.running = 0;
        p.tests.passed = 1;
        p.tests.failed = 1;
        let text = p.render(false);
        assert!(text.contains("Test commands: 1 passed, 1 failed"));
        assert!(text.contains("Setup attempts: 1 passed, 1 failed"));
        assert!(text.contains("base failures"));
        assert!(!text.contains("Testing blocked"));
        assert!(Progress::default().render(false).contains("Disabled"));
    }

    #[test]
    fn receipt_polling_preserves_running_and_excludes_output() {
        let temp = tempfile::tempdir().unwrap();
        let job =
            json!({"id":"job1","repo":"owner/repo","settings":{"execution":{"automatic":true}}});
        let dir = temp.path().join("reviews/job1/experiments");
        assert_eq!(Progress::read(temp.path(), &job).unwrap().tests.total(), 0);
        crate::util::atomic(&dir.join("one.json"), &json!({"phase":"test","status":"running","stdout":"secret output","command":"private command"})).unwrap();
        let live = Progress::read(temp.path(), &job).unwrap();
        assert_eq!(live.tests.running, 1);
        assert!(!serde_json::to_string(&live).unwrap().contains("secret"));
        crate::util::atomic(
            &dir.join("one.json"),
            &json!({"phase":"test","status":"timed_out"}),
        )
        .unwrap();
        assert_eq!(
            Progress::read(temp.path(), &job).unwrap().tests.timed_out,
            1
        );
        let mut invalid = serde_json::to_value(live).unwrap();
        invalid["tests"]["running"] = json!(51);
        assert!(Progress::validate(&invalid).is_err());
        invalid["tests"]["running"] = json!(1);
        invalid["enabled"] = json!(false);
        assert!(Progress::validate(&invalid).is_err());
    }

    #[test]
    fn runtime_errors_show_stages_without_publishing_private_details() {
        let temp = tempfile::tempdir().unwrap();
        let job =
            json!({"id":"job1","repo":"owner/repo","settings":{"execution":{"automatic":true}}});
        let dir = temp.path().join("reviews/job1/experiments");
        crate::util::atomic(
            &dir.join("one.json"),
            &json!({
                "phase":"test", "status":"error", "failureStage":"container_start",
                "error":"/private/operator/token: secret", "cleanupError":"private cleanup error",
                "artifacts":[{"saved":false,"error":"private screenshot failure"}]
            }),
        )
        .unwrap();
        let progress = Progress::read(temp.path(), &job).unwrap();
        let text = progress.render(false);
        assert!(text.contains("1 blocked"));
        assert!(text.contains("container startup"));
        assert!(text.contains("container cleanup"));
        assert!(text.contains("screenshot collection"));
        assert!(!text.contains("private"));
        assert!(!serde_json::to_string(&progress).unwrap().contains("secret"));
        let mut value = serde_json::to_value(progress).unwrap();
        value["failures"] = json!({"/private/operator/token":1});
        assert!(Progress::validate(&value).is_err());
        value["failures"] = json!({"container_start":2});
        assert!(Progress::validate(&value).is_err());
    }

    #[test]
    fn cache_warnings_remain_visible_and_recovered_cleanup_is_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let job =
            json!({"id":"job1","repo":"owner/repo","settings":{"execution":{"automatic":true}}});
        let path = temp.path().join("reviews/job1/experiments/one.json");
        let mut receipt = json!({"phase":"setup","status":"passed",
            "cacheRestoreError":"private restore detail", "cacheSaveError":"private save detail",
            "cleanupError":"private cleanup detail", "artifacts":[{"saved":false},{"saved":false},{"saved":false}]});
        crate::util::atomic(&path, &receipt).unwrap();
        let progress = Progress::read(temp.path(), &job).unwrap();
        assert_eq!(progress.warnings.values().sum::<u32>(), 6);
        let text = progress.render(false);
        assert!(text.contains("dependency cache restore"));
        assert!(text.contains("dependency cache save"));
        assert!(text.contains("container cleanup"));
        assert!(!text.contains("private"));
        receipt["cleanupRecoveredAt"] = json!("2026-09-18T00:00:00Z");
        crate::util::atomic(&path, &receipt).unwrap();
        let recovered = Progress::read(temp.path(), &job).unwrap();
        assert_eq!(recovered.warnings.values().sum::<u32>(), 5);
        assert!(!recovered.render(false).contains("container cleanup"));
        assert!(recovered.render(false).contains("dependency cache save"));
    }
}
