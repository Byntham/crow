//! Check the output a user or script receives from the shipped executable.
use anyhow::Result;
use axum::{Json, Router, extract::State, http::Uri};
use crow::{config, util};
use serde_json::{Value, json};
use std::{collections::HashMap, process::Output, time::Duration};

struct Fixture {
    root: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(role: &str, status: Value) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let responses = HashMap::from([
            ("/admin/status".to_owned(), status),
            ("/admin/pair".to_owned(), json!({"ok": true})),
            ("/admin/cleanup".to_owned(), json!({"jobs": 4, "events": 2})),
            (
                "/worker/maintenance".to_owned(),
                json!({"retentionDays": 7, "jobs": []}),
            ),
        ]);
        let app =
            Router::new()
                .fallback(
                    |State(responses): State<HashMap<String, Value>>, uri: Uri| async move {
                        Json(responses.get(uri.path()).cloned().unwrap_or_else(|| {
                            panic!("Unexpected request to fixture server: {uri}")
                        }))
                    },
                )
                .with_state(responses);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut cfg = config::defaults(root.path());
        cfg["role"] = json!(role);
        cfg["serviceUrl"] = json!(format!("http://{address}"));
        cfg["publicUrl"] = Value::Null;
        config::save(root.path(), &cfg)?;
        // A cached result keeps every test offline and checks that update notices
        // cannot add a second document or prose to JSON output.
        util::atomic(
            &root.path().join("update-status.json"),
            &json!({"checkedAt": util::now(), "available": true, "version": "99.0.0"}),
        )?;
        Ok(Self { root, server })
    }

    async fn run(&self, args: &[&str]) -> Result<Output> {
        let binary = std::env::var("CROW_TEST_BINARY")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_crow").to_owned());
        Ok(tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(binary)
                .args(args)
                .env("CROW_HOME", self.root.path())
                .env("PATH", "")
                .env("NO_COLOR", "1")
                .output(),
        )
        .await??)
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn empty_status() -> Value {
    json!({"repos": [], "workers": [], "jobs": [], "draining": false})
}

fn busy_status() -> Value {
    let now = util::now();
    json!({
        "repos": [{"name": "acme/widgets", "worker": "worker-visible", "policy": "selected", "authors": ["alice"], "settings": {"model": "internal-model-selection"}}],
        "workers": [{"id": "worker-visible", "lastSeen": now, "capacity": 3}],
        "jobs": [{"id": "internal-job-identity", "key": "internal-job-key", "repo": "acme/widgets", "number": 42, "state": "reviewing", "updatedAt": now, "createdAt": now,
            "settings": {"model": "internal-model-selection", "token": "private-setting-token"}, "session": "internal-session-identity", "head": "0123456789abcdef0123456789abcdef01234567"}],
        "draining": false
    })
}

#[tokio::test]
async fn status_is_readable_by_default_and_json_preserves_the_payload() -> Result<()> {
    let status = busy_status();
    let fixture = Fixture::new("service", status.clone()).await?;
    let human = stdout(&fixture.run(&["status"]).await?);
    assert!(human.contains("acme/widgets"), "{human}");
    assert!(human.contains("42"), "{human}");
    assert!(human.to_lowercase().contains("reviewing"), "{human}");
    for hidden in [
        "internal-job-identity",
        "internal-job-key",
        "internal-session-identity",
        "internal-model-selection",
        "private-setting-token",
        "0123456789abcdef0123456789abcdef01234567",
    ] {
        assert!(!human.contains(hidden), "Status exposes {hidden}: {human}");
    }
    assert!(human.lines().count() < 40, "Status is too long: {human}");
    for args in [
        vec!["status", "--format", "json"],
        vec!["--format", "json", "status"],
    ] {
        let machine = stdout(&fixture.run(&args).await?);
        assert_eq!(serde_json::from_str::<Value>(&machine)?, status);
    }
    Ok(())
}

#[tokio::test]
async fn empty_status_explains_how_to_begin() -> Result<()> {
    let fixture = Fixture::new("service", empty_status()).await?;
    let human = stdout(&fixture.run(&["status"]).await?);
    assert!(human.contains("crow enroll"), "{human}");
    assert!(!human.contains("\"repos\""), "{human}");
    assert!(!human.contains("[]"), "{human}");
    Ok(())
}

#[tokio::test]
async fn worker_status_uses_local_state_and_keeps_json_available() -> Result<()> {
    let fixture = Fixture::new("worker", empty_status()).await?;
    let state = json!({"version": 1, "pid": std::process::id(), "updatedAt": util::now(), "state": "running", "connection": "connected", "active": [
        {"id": "internal-worker-job", "repo": "acme/widgets", "number": 17, "state": "reviewing"}
    ]});
    util::atomic(&fixture.root.path().join("worker-status.json"), &state)?;
    util::atomic(
        &fixture.root.path().join("runtime.lock"),
        &json!({"pid": std::process::id()}),
    )?;
    let human = stdout(&fixture.run(&["status"]).await?);
    assert!(human.to_lowercase().contains("running"), "{human}");
    assert!(human.contains("acme/widgets"), "{human}");
    assert!(!human.contains("internal-worker-job"), "{human}");
    let machine = stdout(&fixture.run(&["status", "--format", "json"]).await?);
    assert_eq!(serde_json::from_str::<Value>(&machine)?, state);
    std::fs::remove_file(fixture.root.path().join("runtime.lock"))?;
    let stopped = stdout(&fixture.run(&["status"]).await?);
    assert!(stopped.contains("not running"), "{stopped}");
    assert!(!stopped.contains("acme/widgets"), "{stopped}");
    std::fs::remove_file(fixture.root.path().join("worker-status.json"))?;
    let missing = stdout(&fixture.run(&["status"]).await?);
    assert!(missing.contains("crow doctor"), "{missing}");
    Ok(())
}

#[tokio::test]
async fn doctor_reports_failures_and_preserves_a_failing_exit_code() -> Result<()> {
    let fixture = Fixture::new("service", busy_status()).await?;
    let human = fixture.run(&["doctor"]).await?;
    assert!(!human.status.success());
    let text = String::from_utf8(human.stdout)?;
    assert!(text.contains("GitHub App"), "{text}");
    assert!(!text.contains("internal-job-identity"), "{text}");
    assert!(!text.contains("\"checks\""), "{text}");
    let machine = fixture.run(&["doctor", "--format", "json"]).await?;
    assert!(!machine.status.success());
    let result: Value = serde_json::from_slice(&machine.stdout)?;
    assert_eq!(result["ok"], false);
    assert!(
        result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["ok"] == false)
    );
    Ok(())
}

#[tokio::test]
async fn config_accepts_plain_model_names_and_redacts_credentials() -> Result<()> {
    // A service host validates settings locally without calling a model provider.
    let fixture = Fixture::new("service", empty_status()).await?;
    stdout(
        &fixture
            .run(&["config", "worker.model", "review-model"])
            .await?,
    );
    stdout(&fixture.run(&["config", "worker.effort", "high"]).await?);
    let mut saved = config::load(fixture.root.path())?;
    assert_eq!(saved["worker"]["model"], "review-model");
    assert_eq!(saved["worker"]["effort"], "high");
    saved["role"] = json!("both");
    config::save(fixture.root.path(), &saved)?;
    let human = stdout(&fixture.run(&["config"]).await?);
    assert!(human.contains("review-model"), "{human}");
    assert!(!human.contains(saved["adminToken"].as_str().unwrap()));
    assert!(!human.contains(saved["worker"]["token"].as_str().unwrap()));
    let machine = stdout(&fixture.run(&["config", "--format", "json"]).await?);
    let redacted: Value = serde_json::from_str(&machine)?;
    assert_eq!(redacted["worker"]["model"], "review-model");
    assert_eq!(redacted["adminToken"], "[hidden]");
    assert_eq!(redacted["worker"]["token"], "[hidden]");
    Ok(())
}

#[tokio::test]
async fn interrupted_app_connection_never_exposes_credentials_in_config_output() -> Result<()> {
    let fixture = Fixture::new("service", empty_status()).await?;
    let mut saved = config::load(fixture.root.path())?;
    saved["publicUrl"] = json!("https://crow.example");
    saved["pendingApp"] = json!({
        "app":{"id":42,"slug":"existing-crow","pem":"private-pem-must-not-leak","webhookSecret":"webhook-secret-must-not-leak"},
        "webhookUrl":"https://crow.example/webhooks/github"
    });
    config::save(fixture.root.path(), &saved)?;
    for args in [vec!["config"], vec!["config", "--format", "json"]] {
        let output = fixture.run(&args).await?;
        let text = stdout(&output);
        for secret in ["private-pem-must-not-leak", "webhook-secret-must-not-leak"] {
            assert!(!text.contains(secret));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        }
        if args.len() > 1 {
            let redacted: Value = serde_json::from_str(&text)?;
            assert_eq!(redacted["pendingApp"]["app"]["pem"], "[hidden]");
            assert_eq!(redacted["pendingApp"]["app"]["webhookSecret"], "[hidden]");
            assert_eq!(redacted["pendingApp"]["app"]["id"], 42);
        }
    }
    assert_eq!(config::load(fixture.root.path())?, saved);
    Ok(())
}

#[tokio::test]
async fn pair_and_combined_cleanup_each_emit_one_json_document() -> Result<()> {
    let fixture = Fixture::new("both", empty_status()).await?;
    let pairing = stdout(&fixture.run(&["pair", "--format", "json"]).await?);
    let pairing: Value = serde_json::from_str(&pairing)?;
    assert!(pairing.get("serviceUrl").is_some());
    assert!(!pairing["id"].as_str().unwrap().is_empty());
    assert!(pairing["token"].as_str().unwrap().len() >= 32);
    let cleanup = stdout(&fixture.run(&["cleanup", "--format", "json"]).await?);
    let cleanup: Value = serde_json::from_str(&cleanup)?;
    assert_eq!(cleanup["service"], json!({"jobs": 4, "events": 2}));
    assert_eq!(cleanup["worker"], json!({"removed": [], "warnings": []}));
    Ok(())
}

#[tokio::test]
async fn help_explains_every_public_command_and_hides_internal_entry_points() -> Result<()> {
    let fixture = Fixture::new("service", empty_status()).await?;
    for args in [vec![], vec!["-h"], vec!["--help"], vec!["help"]] {
        let help = stdout(&fixture.run(&args).await?);
        assert!(help.contains("crow setup"), "{help}");
        assert!(help.contains("--format"), "{help}");
        assert!(!help.contains("_inspection-mcp"), "{help}");
        assert!(!help.contains('\u{1b}'), "NO_COLOR must disable styling");
        let headings = [
            "Get started:",
            "Setup and workers:",
            "Repositories:",
            "Reviews:",
            "Status and troubleshooting:",
            "Service:",
            "Settings and maintenance:",
            "Options:",
        ];
        let mut remaining = help.as_str();
        for heading in headings {
            remaining = remaining.split_once(heading).expect(heading).1;
        }
        let commands = help
            .split_once("Setup and workers:\n")
            .unwrap()
            .1
            .split_once("\nOptions:")
            .unwrap()
            .0;
        for line in commands.lines().filter(|line| line.starts_with("  ")) {
            assert!(
                line.split_whitespace().count() > 2,
                "Command has no useful description: {line}"
            );
        }
        let reviews = help
            .split_once("Reviews:\n")
            .unwrap()
            .1
            .split_once("\nStatus and troubleshooting:")
            .unwrap()
            .0;
        assert!(reviews.contains("  restart "), "{reviews}");
        assert!(reviews.contains("  release "), "{reviews}");
        assert!(!reviews.contains("service-restart"), "{reviews}");
    }
    for command in ["enroll", "review", "repo-config", "config", "pair"] {
        let detail = stdout(&fixture.run(&[command, "--help"]).await?);
        assert!(detail.contains("Usage:"), "{detail}");
        assert!(detail.contains("--format"), "{detail}");
    }
    Ok(())
}
