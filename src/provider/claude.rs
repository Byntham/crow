//! Claude Code reviews through `claude -p` with streamed JSON events.
//!
//! Crow gives Claude Code a dedicated `CLAUDE_CONFIG_DIR` and subscription
//! login, removes every built-in tool, loads no user, project, or local
//! settings, hooks, skills, or CLAUDE.md files, and exposes only Crow's
//! inspection MCP server through `dontAsk` permissions. Claude Code reports the
//! effective tools, MCP servers, model, and credential source when a session
//! starts; Crow stops the review if any of them differ from what it requested.
use super::{
    BOUNDARY, Check, Command, Environment, Event, Layout, ProviderError, classify_error,
    executable, failure, find_model, isolated_environment, text, user_home,
};
use crate::process::{self, RunOptions};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Claude Code environment switches that keep personal or automatic context
/// out of reviews and stop background network traffic.
const ENVIRONMENT: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    // Diff and file-list pages hold up to 200,000 characters.
    ("MAX_MCP_OUTPUT_TOKENS", "120000"),
    // Git inspection calls may take up to two minutes.
    ("MCP_TOOL_TIMEOUT", "300000"),
];
/// Flags required by every Crow invocation of `claude -p`.
const REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--output-format",
    "--input-format",
    "--json-schema",
    "--mcp-config",
    "--strict-mcp-config",
    "--permission-mode",
    "--allowedTools",
    "--tools",
    "--setting-sources",
    "--settings",
    "--disable-slash-commands",
    "--system-prompt",
    "--resume",
    "--model",
    "--effort",
];
const NO_HOOKS: &str = r#"{"disableAllHooks":true}"#;
/// Reasoning level for models without adjustable effort.
const MODEL_DEFAULT_EFFORT: &str = "default";

fn program(settings: &Value) -> &str {
    executable(settings, "claude", "claude")
}
/// Arguments that load no tools, settings, hooks, skills, or MCP servers
/// beyond those Crow passes explicitly.
fn locked_down() -> Vec<String> {
    [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--settings",
        NO_HOOKS,
        "--disable-slash-commands",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub(super) fn environment(settings: &Value, root: &Path) -> Result<Environment> {
    let (_, mut env) = isolated_environment(
        root,
        "claudeHome",
        "CLAUDE_CONFIG_DIR",
        "claude",
        &user_home()?.join(".claude"),
        settings,
    )?;
    for (key, value) in ENVIRONMENT {
        env.insert((*key).into(), (*value).into());
    }
    // An experiment can outlast the default MCP tool timeout.
    if let Some(policy) = settings.get("execution").filter(|p| p.is_object()) {
        let seconds = crate::execution::tool_timeout_seconds(policy);
        env.insert("MCP_TOOL_TIMEOUT".into(), (seconds * 1000).to_string());
    }
    // Network settings Claude Code needs to reach Anthropic from this host.
    for key in [
        "NODE_EXTRA_CA_CERTS",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(key).filter(|v| !v.is_empty()) {
            env.insert(key.into(), value.to_string_lossy().into_owned());
        }
    }
    Ok(Environment {
        env,
        settings: settings.clone(),
    })
}
async fn run(
    settings: &Value,
    environment: &Environment,
    args: Vec<String>,
    input: Option<String>,
    cancel: &CancellationToken,
) -> Result<process::Output> {
    process::run(
        program(settings),
        &args,
        RunOptions {
            cwd: Some(PathBuf::from(&environment.env["HOME"])),
            env: Some(environment.env.clone()),
            input,
            cancel: cancel.clone(),
            timeout: Some(Duration::from_secs(30)),
            check: false,
            ..Default::default()
        },
    )
    .await
}

pub(super) async fn auth(
    settings: &Value,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Value> {
    let environment = environment(settings, root)?;
    let args = vec!["auth".into(), "status".into(), "--json".into()];
    let output = run(settings, &environment, args, None, cancel).await?;
    let status: Value = serde_json::from_str(output.stdout.trim()).map_err(|_| {
        anyhow!(
            "Claude Code did not report its authentication status: {}",
            output.stderr.trim()
        )
    })?;
    let method = text(&status, "authMethod");
    let logged_in = status["loggedIn"] == true;
    let subscription = logged_in
        && status["apiProvider"] == "firstParty"
        && matches!(method, "claude.ai" | "oauth_token");
    let warning = if subscription {
        None
    } else if logged_in {
        Some(
            "Claude Code is using API-key or cloud-provider billing. Crow only uses a Claude subscription; run crow login.",
        )
    } else {
        Some("Sign in to a Claude subscription with crow login.")
    };
    Ok(json!({
        "authenticated": subscription,
        "account": {"type": method, "planType": status["subscriptionType"]},
        "warning": warning,
    }))
}

/// Read the model catalog from Claude Code's SDK `initialize` handshake. It
/// reports the account and available models without starting a conversation.
pub(super) async fn catalog(
    settings: &Value,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Value> {
    // The handshake also answers for API-key logins; list models only for a subscription.
    let status = auth(settings, root, cancel).await?;
    if status["authenticated"] != true {
        return Err(failure(
            "auth",
            status["warning"]
                .as_str()
                .unwrap_or("A Claude subscription login is required for model discovery."),
        ));
    }
    let environment = environment(settings, root)?;
    let mut args = locked_down();
    args.extend(["--input-format".into(), "stream-json".into()]);
    args.push("--no-session-persistence".into());
    let request = json!({
        "type": "control_request",
        "request_id": "crow-initialize",
        "request": {"subtype": "initialize"},
    });
    let output = run(
        settings,
        &environment,
        args,
        Some(format!("{request}\n")),
        cancel,
    )
    .await?;
    let response = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|m| {
            m["type"] == "control_response" && m["response"]["request_id"] == "crow-initialize"
        })
        .map(|m| m["response"].clone())
        .with_context(|| {
            format!(
                "Claude Code did not report its models: {}",
                output.stderr.trim()
            )
        })?;
    if response["subtype"] != "success" {
        bail!(
            "Claude Code could not report its models: {}",
            text(&response, "error")
        );
    }
    let account = &response["response"]["account"];
    if account["tokenSource"] == "none" {
        return Err(failure(
            "auth",
            "A Claude subscription login is required for model discovery. Run crow login.",
        ));
    }
    if account["apiProvider"] != "firstParty" {
        return Err(failure(
            "auth",
            "Claude Code is configured for a cloud provider. Crow only uses a Claude subscription.",
        ));
    }
    let models = response["response"]["models"]
        .as_array()
        .context("Claude Code returned an invalid model catalog")?;
    Ok(json!({
        "models": normalize_models(models),
        "account": {"type": "claude.ai", "planType": account["subscriptionType"]},
    }))
}
/// Convert Claude Code's model list into Crow's catalog shape. Aliases such as
/// `opus` keep following Anthropic's current release; the pinned model each one
/// resolves to is accepted as well. The `default` pseudo-model is not listed;
/// the entry it resolves to is marked as the default instead.
fn normalize_models(models: &[Value]) -> Vec<Value> {
    let default = models
        .iter()
        .find(|m| m["value"] == "default")
        .and_then(|m| m["resolvedModel"].as_str());
    let mut marked = false;
    let mut result = Vec::new();
    for model in models {
        let Some(value) = model["value"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        if value == "default" {
            continue;
        }
        let efforts: Vec<&str> = if model["supportsEffort"] == true {
            model["supportedEffortLevels"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default()
        } else {
            vec![]
        };
        let (supported, default_effort) = if efforts.is_empty() {
            (
                vec![json!({
                    "reasoningEffort": MODEL_DEFAULT_EFFORT,
                    "description": "This model has no adjustable reasoning level.",
                })],
                MODEL_DEFAULT_EFFORT,
            )
        } else {
            let preferred = if efforts.contains(&"high") {
                "high"
            } else {
                efforts[0]
            };
            (
                efforts
                    .iter()
                    .map(|e| json!({"reasoningEffort": e}))
                    .collect(),
                preferred,
            )
        };
        let is_default = !marked && default.is_some() && model["resolvedModel"].as_str() == default;
        marked |= is_default;
        result.push(json!({
            "model": value,
            "displayName": model["description"].as_str().or(model["displayName"].as_str()).unwrap_or(value),
            "resolvedModel": model["resolvedModel"],
            "isDefault": is_default,
            "defaultReasoningEffort": default_effort,
            "supportedReasoningEfforts": supported,
        }));
    }
    result
}

pub(super) async fn login(settings: &Value, root: &Path) -> Result<()> {
    let environment = environment(settings, root)?;
    process::run(
        program(settings),
        &["auth".into(), "login".into(), "--claudeai".into()],
        RunOptions {
            cwd: Some(PathBuf::from(&environment.env["HOME"])),
            env: Some(environment.env),
            inherit: true,
            detached: false,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn checks(settings: &Value, root: &Path) -> Result<Vec<Check>> {
    let environment = environment(settings, root)?;
    let cancel = CancellationToken::new();
    let succeeded = |output: process::Output| -> Result<String> {
        if output.stdout.trim().is_empty() {
            bail!("{}", output.stderr.trim());
        }
        Ok(output.stdout)
    };
    let version = run(
        settings,
        &environment,
        vec!["--version".into()],
        None,
        &cancel,
    )
    .await
    .and_then(succeeded)
    .and_then(|out| {
        let version = out.trim().to_owned();
        if !version.contains("Claude Code") {
            bail!("{version} is not the official Claude Code CLI.");
        }
        Ok(version)
    });
    let controls = run(settings, &environment, vec!["--help".into()], None, &cancel)
        .await
        .and_then(succeeded)
        .and_then(|help| {
            for flag in REQUIRED_FLAGS {
                if !help.contains(flag) {
                    bail!("Installed Claude Code lacks {flag}; update it with claude update.");
                }
            }
            Ok(format!(
                "All {} required headless review controls are available.",
                REQUIRED_FLAGS.len()
            ))
        });
    Ok(vec![
        Check {
            name: "Claude Code executable",
            result: version,
        },
        Check {
            name: "Headless review controls",
            result: controls,
        },
    ])
}

#[derive(Default)]
struct ParseState {
    /// The specific API error behind a failed turn, reported before the result.
    error: Option<ProviderError>,
    /// When a rejected rate limit resets, in Unix milliseconds.
    resets_at: Option<u64>,
}
pub(super) struct Review {
    env: BTreeMap<String, String>,
    settings: Value,
    mcp_config: PathBuf,
    /// Fully qualified MCP tool names the reviewer may call.
    tools: Vec<String>,
    /// The model Claude Code should report once the session starts, when known.
    expected_model: Option<String>,
    state: Mutex<ParseState>,
}
impl Review {
    pub(super) fn prepare(
        layout: &Layout,
        environment: Environment,
        models: &[Value],
        cached: bool,
    ) -> Result<Self> {
        let settings = environment.settings;
        let model = text(&settings, "model");
        let resolved = find_model(models, model)
            .and_then(|m| m["resolvedModel"].as_str())
            .unwrap_or(model);
        // A cached catalog may predate an alias moving to a newer model, so
        // only a pinned model can be checked against it.
        let expected_model = (!cached || resolved == model).then(|| resolved.to_owned());
        let helper = std::env::current_exe()?;
        // The helper runs with the same isolated, credential-free environment.
        let mcp = json!({"mcpServers": {"crow_inspection": {
            "type": "stdio",
            "command": helper,
            "args": ["_inspection-mcp", layout.source_path, layout.context_path],
            "env": environment.env,
        }}});
        let mcp_config = layout.dir.join("mcp.json");
        crate::util::atomic(&mcp_config, &mcp)?;
        Ok(Self {
            tools: layout
                .tools
                .iter()
                .map(|name| format!("mcp__crow_inspection__{name}"))
                .collect(),
            env: environment.env,
            settings,
            mcp_config,
            expected_model,
            state: Mutex::default(),
        })
    }
    pub(super) fn command(&self, _layout: &Layout, session: Option<&str>) -> Result<Command> {
        let mut args = locked_down();
        args.extend(["--model".into(), text(&self.settings, "model").into()]);
        let effort = text(&self.settings, "effort");
        if effort != MODEL_DEFAULT_EFFORT {
            args.extend(["--effort".into(), effort.into()]);
        }
        args.extend([
            "--mcp-config".into(),
            self.mcp_config.to_string_lossy().into_owned(),
            "--permission-mode".into(),
            "dontAsk".into(),
            "--allowedTools".into(),
            self.tools.join(","),
            "--system-prompt".into(),
            BOUNDARY.into(),
            "--json-schema".into(),
            crate::report::schema().to_string(),
        ]);
        if let Some(session) = session {
            args.extend(["--resume".into(), session.into()]);
        }
        Ok(Command {
            program: program(&self.settings).into(),
            args,
            env: self.env.clone(),
        })
    }
    pub(super) fn parse(&self, event: &Value) -> Result<Vec<Event>> {
        let mut state = self.state.lock().unwrap();
        Ok(match (text(event, "type"), text(event, "subtype")) {
            ("system", "init") => {
                self.verify(event)?;
                vec![Event::Session(text(event, "session_id").into())]
            }
            ("assistant", _) => {
                if let Some(code) = event["error"].as_str() {
                    let message = content_text(&event["message"]);
                    state.error = Some(api_error(code, &message, state.resets_at));
                }
                let mut events = Vec::new();
                for item in event["message"]["content"].as_array().into_iter().flatten() {
                    match text(item, "type") {
                        "text" => {
                            events.push(Event::Message(text(item, "text").into()));
                            events.push(Event::Progress("agent_message"));
                        }
                        "tool_use" => events.push(Event::Progress("mcp_tool_call")),
                        "thinking" | "redacted_thinking" => {
                            events.push(Event::Progress("reasoning"))
                        }
                        _ => {}
                    }
                }
                events
            }
            ("rate_limit_event", _) => {
                let info = &event["rate_limit_info"];
                if info["status"] == "rejected" {
                    state.resets_at = info["resetsAt"].as_u64().map(|s| s.saturating_mul(1000));
                }
                vec![]
            }
            ("result", subtype) => {
                if event["is_error"] == false && subtype == "success" {
                    let output = if event["structured_output"].is_object() {
                        event["structured_output"].to_string()
                    } else {
                        text(event, "result").to_owned()
                    };
                    return Ok(vec![Event::Output(output), Event::Completed]);
                }
                let message = event["result"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        event["errors"].as_array().map(|errors| {
                            errors
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| format!("Claude Code review ended with {subtype}"));
                let mut error = state
                    .error
                    .take()
                    .unwrap_or_else(|| classify_error(&anyhow!(message)));
                if subtype == "error_max_structured_output_retries" {
                    error.kind = "output".into();
                }
                vec![Event::Failed(error)]
            }
            _ => vec![],
        })
    }
    /// Check the effective session against Crow's request before any tool runs.
    fn verify(&self, init: &Value) -> Result<()> {
        let tools: Vec<&str> = init["tools"]
            .as_array()
            .context("Claude Code did not report its tools")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let unexpected: Vec<&str> = tools
            .iter()
            .copied()
            .filter(|t| *t != "StructuredOutput" && !self.tools.iter().any(|a| a == t))
            .collect();
        if !unexpected.is_empty() {
            return Err(failure(
                "config",
                format!(
                    "Claude Code offered tools outside Crow's inspection boundary: {}. Check host-managed Claude Code settings.",
                    unexpected.join(", ")
                ),
            ));
        }
        let servers = init["mcp_servers"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        if servers.len() != 1 || servers[0]["name"] != "crow_inspection" {
            return Err(failure(
                "config",
                "Unexpected MCP servers are configured outside Crow. Check host-managed Claude Code settings.",
            ));
        }
        if servers[0]["status"] != "connected"
            || self.tools.iter().any(|t| !tools.contains(&t.as_str()))
        {
            return Err(failure(
                "transient",
                "Crow's inspection tools did not start in Claude Code.",
            ));
        }
        if init["permissionMode"] != "dontAsk" {
            return Err(failure(
                "config",
                "Claude Code did not apply Crow's permission mode. Check host-managed Claude Code settings.",
            ));
        }
        if init["apiKeySource"] != "none" {
            return Err(failure(
                "auth",
                "Claude Code is using an API key. Crow only uses a Claude subscription; run crow login.",
            ));
        }
        let base = |model: &str| model.split('[').next().unwrap_or(model).to_owned();
        if let Some(expected) = &self.expected_model
            && base(text(init, "model")) != base(expected)
        {
            return Err(failure(
                "config",
                format!(
                    "Claude Code started {} instead of the requested {expected}. Check host-managed Claude Code settings.",
                    text(init, "model"),
                ),
            ));
        }
        Ok(())
    }
}
fn content_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}
/// Map Claude Code's API error codes. Subscription usage limits and billing
/// problems pause for the operator; the reset time becomes the retry delay.
/// Claude Code reports every HTTP 429 as `rate_limit`, so only a rate limit it
/// also reported as rejected is a usage limit; short throttling stays transient.
fn api_error(code: &str, message: &str, resets_at: Option<u64>) -> ProviderError {
    let message = if message.is_empty() {
        format!("Claude Code API error: {code}")
    } else {
        message.to_owned()
    };
    let mut error = classify_error(&anyhow!(message));
    match code {
        "authentication_failed" => error.kind = "auth".into(),
        "billing_error" => error.kind = "quota".into(),
        "rate_limit" if resets_at.is_some() => error.kind = "quota".into(),
        _ => {}
    }
    if error.kind == "quota"
        && let Some(reset) = resets_at
    {
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        error.retry_after = error.retry_after.max(reset.saturating_sub(now));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::super::{Callbacks, diagnostics, discover, run_review};
    use super::*;
    use std::sync::Arc;

    fn compiled_fake() -> PathBuf {
        super::super::fixtures::compile(
            "claude",
            include_str!("../../tests/fixtures/fake_claude.rs"),
        )
    }
    struct Fixture {
        root: tempfile::TempDir,
        job: Value,
        source: Value,
    }
    impl Fixture {
        fn new(behavior: Value) -> Self {
            let root = tempfile::tempdir().unwrap();
            let claude = root.path().join("fake-claude");
            std::fs::hard_link(compiled_fake(), &claude).unwrap();
            crate::util::atomic(&root.path().join("behavior.json"), &behavior).unwrap();
            let source = json!({
                "dir": root.path().join("source"),
                "head": "a".repeat(40),
                "base": "b".repeat(40),
                "target": "main",
                "targetSha": "c".repeat(40),
            });
            let job = json!({
                "id": "job-one",
                "repo": "owner/repo",
                "number": 1,
                "comparison": source,
                "settings": {
                    "provider": "claude",
                    "claude": claude,
                    "claudeHome": root.path().join("claude"),
                    "model": "opus",
                    "effort": "high",
                    "subagents": {"mode": "inherit", "max": 8},
                    "retry": {"mode": "fixed", "count": 10, "delayMs": 5000},
                },
            });
            Self { root, job, source }
        }
        fn set(&self, behavior: Value) {
            crate::util::atomic(&self.root.path().join("behavior.json"), &behavior).unwrap();
        }
        fn invocation(&self) -> Value {
            crate::util::read_json(&self.root.path().join("invocation.json"))
                .unwrap()
                .unwrap()
        }
        async fn run(&self) -> Result<Value> {
            run_review(
                &self.job,
                &self.source,
                &json!({"files":[{"path":"AGENTS.md","body":"Target guidance"}]}),
                self.root.path(),
                Callbacks::default(),
                CancellationToken::new(),
            )
            .await
        }
    }
    fn kind(error: anyhow::Error) -> String {
        classify_error(&error).kind
    }
    fn flag<'a>(args: &'a [Value], name: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == name)?;
        args.get(at + 1).and_then(Value::as_str)
    }

    #[tokio::test]
    async fn catalog_lists_aliases_marks_default_and_never_invents_models() {
        let f = Fixture::new(json!({}));
        let catalog = discover(&f.job["settings"], f.root.path()).await.unwrap();
        let models = catalog["models"].as_array().unwrap();
        let names: Vec<_> = models.iter().map(|m| m["model"].clone()).collect();
        assert_eq!(names, vec![json!("opus"), json!("sonnet"), json!("haiku")]);
        assert_eq!(models[0]["isDefault"], true);
        assert_eq!(models[0]["defaultReasoningEffort"], "high");
        assert_eq!(
            models[2]["supportedReasoningEfforts"],
            json!([{"reasoningEffort":"default","description":"This model has no adjustable reasoning level."}])
        );
        assert_eq!(catalog["cached"], false);
        f.set(json!({"offline":true}));
        let cached = discover(&f.job["settings"], f.root.path()).await.unwrap();
        assert_eq!(cached["cached"], true);
        assert_eq!(cached["models"], catalog["models"]);
        let fresh = Fixture::new(json!({"offline":true}));
        let error = discover(&fresh.job["settings"], fresh.root.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no cached list"));
        let unauthenticated = Fixture::new(json!({"unauth":true}));
        let error = discover(
            &unauthenticated.job["settings"],
            unauthenticated.root.path(),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(error), "auth");
        let api_key = Fixture::new(json!({"apiKey":true}));
        let error = discover(&api_key.job["settings"], api_key.root.path())
            .await
            .unwrap_err();
        assert_eq!(kind(error), "auth");
    }
    #[tokio::test]
    async fn diagnostics_check_headless_controls_and_catalog() {
        let f = Fixture::new(json!({}));
        let result = diagnostics(&f.job["settings"], f.root.path(), true)
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["provider"], "Claude Code");
        assert_eq!(result["version"], "2.1.289 (Claude Code)");
        assert!(result.to_string().contains("3 models available"));
        let old = Fixture::new(json!({"oldHelp":true}));
        let result = diagnostics(&old.job["settings"], old.root.path(), false)
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert!(result.to_string().contains("--disable-slash-commands"));
    }
    #[tokio::test]
    async fn subscription_status_rejects_api_keys_and_missing_login() {
        for (behavior, authenticated) in [
            (json!({}), true),
            (json!({"unauth":true}), false),
            (json!({"apiKey":true}), false),
        ] {
            let f = Fixture::new(behavior);
            let status = super::super::auth_status(&f.job["settings"], f.root.path())
                .await
                .unwrap();
            assert_eq!(status["authenticated"], authenticated, "{status}");
        }
    }
    #[tokio::test]
    async fn review_runs_locked_down_and_persists_session_before_callback() {
        let f = Fixture::new(json!({}));
        let file = f.root.path().join("reviews/job-one/session.json");
        let called = Arc::new(Mutex::new(Vec::new()));
        let sessions = called.clone();
        let callbacks = Callbacks {
            on_session: Some(Arc::new(move |id| {
                let file = file.clone();
                let sessions = sessions.clone();
                Box::pin(async move {
                    assert_eq!(crate::util::read_json(&file)?.unwrap()["id"], id);
                    sessions.lock().unwrap().push(id);
                    Ok(())
                })
            })),
            on_progress: None,
        };
        let report = run_review(
            &f.job,
            &f.source,
            &json!({"files":[{"path":"AGENTS.md","body":"Target guidance"}]}),
            f.root.path(),
            callbacks,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(report["findings"], json!([]));
        let session = "11111111-2222-4333-8444-555555555555";
        assert_eq!(*called.lock().unwrap(), vec![json!(session)]);
        let saved = crate::util::read_json(&f.root.path().join("reviews/job-one/session.json"))
            .unwrap()
            .unwrap();
        assert_eq!(saved["provider"], "claude");
        let invocation = f.invocation();
        let args = invocation["args"].as_array().unwrap();
        assert_eq!(flag(args, "--tools"), Some(""));
        assert_eq!(flag(args, "--setting-sources"), Some(""));
        assert_eq!(flag(args, "--permission-mode"), Some("dontAsk"));
        assert_eq!(flag(args, "--model"), Some("opus"));
        assert_eq!(flag(args, "--effort"), Some("high"));
        assert_eq!(flag(args, "--settings"), Some(NO_HOOKS));
        assert!(args.contains(&json!("--strict-mcp-config")));
        assert!(args.contains(&json!("--disable-slash-commands")));
        assert!(!args.contains(&json!("--resume")));
        let allowed = flag(args, "--allowedTools").unwrap();
        assert!(allowed.contains("mcp__crow_inspection__diff"));
        assert!(allowed.contains("mcp__crow_inspection__start_review_task"));
        let env = &invocation["env"];
        assert_eq!(
            env["CLAUDE_CONFIG_DIR"],
            json!(f.root.path().join("claude"))
        );
        assert_eq!(env["CLAUDE_CODE_DISABLE_CLAUDE_MDS"], "1");
        assert_eq!(env["ANTHROPIC_API_KEY"], Value::Null);
        assert_ne!(env["HOME"], json!(std::env::var("HOME").unwrap()));
        assert_ne!(invocation["cwd"], f.source["dir"]);
        assert!(text(&invocation, "prompt").contains("Target guidance"));
        let mcp = crate::util::read_json(&f.root.path().join("reviews/job-one/mcp.json"))
            .unwrap()
            .unwrap();
        let server = &mcp["mcpServers"]["crow_inspection"];
        assert_eq!(server["args"][0], "_inspection-mcp");
        assert_eq!(server["env"]["CLAUDE_CONFIG_DIR"], env["CLAUDE_CONFIG_DIR"]);
        assert_eq!(
            crate::util::read_json(&f.root.path().join("reviews/job-one/report.json"))
                .unwrap()
                .unwrap(),
            report
        );
    }
    #[tokio::test]
    async fn resume_continues_the_saved_session_and_rejects_replacements() {
        let mut f = Fixture::new(json!({"invalid":true}));
        assert_eq!(kind(f.run().await.unwrap_err()), "output");
        f.set(json!({}));
        f.job["session"] = json!("11111111-2222-4333-8444-555555555555");
        f.run().await.unwrap();
        let invocation = f.invocation();
        let args = invocation["args"].as_array().unwrap();
        assert_eq!(
            flag(args, "--resume"),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert!(text(&invocation, "prompt").starts_with("Continue the incomplete Crow review"));
        f.set(json!({"wrongSession":true}));
        assert_eq!(kind(f.run().await.unwrap_err()), "restart");
        // Claude Code no longer has the conversation, for example after its state was removed.
        f.set(json!({"missingSession":true}));
        assert_eq!(kind(f.run().await.unwrap_err()), "restart");
    }
    #[tokio::test]
    async fn sessions_saved_by_another_provider_require_a_restart() {
        let mut f = Fixture::new(json!({}));
        let session = "22222222-2222-4333-8444-555555555555";
        // Sessions saved before provider selection carry no provider and are Codex sessions.
        crate::util::atomic(
            &f.root.path().join("reviews/job-one/session.json"),
            &json!({"id":session,"comparison":f.source,"settings":{}}),
        )
        .unwrap();
        f.job["session"] = json!(session);
        assert_eq!(kind(f.run().await.unwrap_err()), "restart");
        f.job["session"] = Value::Null;
        assert_eq!(kind(f.run().await.unwrap_err()), "restart");
        assert!(!f.root.path().join("invocation.json").exists());
    }
    #[tokio::test]
    async fn session_policy_violations_stop_the_review() {
        for (behavior, expected) in [
            (json!({"extraTool":"Bash"}), "config"),
            (json!({"wrongModel":true}), "config"),
            (json!({"apiKeySource":"ANTHROPIC_API_KEY"}), "auth"),
            (json!({"permissionMode":"bypassPermissions"}), "config"),
            (json!({"mcpFailed":true}), "transient"),
        ] {
            let f = Fixture::new(behavior.clone());
            assert_eq!(kind(f.run().await.unwrap_err()), expected, "{behavior}");
            assert!(
                !f.root.path().join("reviews/job-one/report.json").exists(),
                "{behavior}"
            );
        }
        for behavior in [json!({"unauth":true}), json!({"apiKey":true})] {
            let f = Fixture::new(behavior);
            assert_eq!(kind(f.run().await.unwrap_err()), "auth");
            assert!(!f.root.path().join("invocation.json").exists());
        }
        let mut f = Fixture::new(json!({}));
        f.job["settings"]["effort"] = json!("unsupported");
        assert_eq!(kind(f.run().await.unwrap_err()), "config");
        assert!(!f.root.path().join("invocation.json").exists());
    }
    #[tokio::test]
    async fn experiments_add_runtime_tools_timeout_and_report_summary() {
        let mut f = Fixture::new(json!({}));
        f.job["settings"]["execution"] = json!({
            "podman": "podman",
            "limits": crate::execution::Limits::default(),
        });
        // A receipt from earlier in this review appears in the published summary.
        let receipt = json!({"id":"1","kind":"test","revision":"head","status":"passed","purpose":"Run the unit tests"});
        let receipt_path = "reviews/job-one/experiments/0001.json";
        crate::util::atomic(&f.root.path().join(receipt_path), &receipt).unwrap();
        let report = f.run().await.unwrap();
        assert!(
            text(&report, "summary").contains("- passed, PR version: Run the unit tests"),
            "{report}"
        );
        let invocation = f.invocation();
        let args = invocation["args"].as_array().unwrap();
        let allowed = flag(args, "--allowedTools").unwrap();
        for name in crate::execution::TOOL_NAMES {
            assert!(allowed.contains(&format!("mcp__crow_inspection__{name}")));
        }
        let timeout = crate::execution::Limits::default().timeout_seconds + 300;
        assert_eq!(
            invocation["env"]["MCP_TOOL_TIMEOUT"],
            json!((timeout * 1000).to_string())
        );
        assert!(text(&invocation, "prompt").contains("Runtime tools are available"));
        let context = crate::util::read_json(
            &f.root
                .path()
                .join("reviews/job-one/delegation-context.json"),
        )
        .unwrap()
        .unwrap();
        assert!(context["job"]["settings"]["execution"].is_object());
        // Without a policy, no runtime tools, prompt or summary.
        let plain = Fixture::new(json!({}));
        plain.run().await.unwrap();
        let invocation = plain.invocation();
        let allowed = flag(invocation["args"].as_array().unwrap(), "--allowedTools").unwrap();
        assert!(!allowed.contains("run_experiment"));
        assert!(!text(&invocation, "prompt").contains("Runtime tools are available"));
        assert!(!text(&plain.run().await.unwrap(), "summary").contains("Runtime checks"));
        // A resumed review that lost its policy still reports what already ran.
        let resumed = Fixture::new(json!({}));
        crate::util::atomic(&resumed.root.path().join(receipt_path), &receipt).unwrap();
        let report = resumed.run().await.unwrap();
        assert!(
            text(&report, "summary").contains("- passed, PR version: Run the unit tests"),
            "{report}"
        );
    }
    #[tokio::test]
    async fn pinned_models_and_fixed_effort_models_are_accepted() {
        let mut f = Fixture::new(json!({}));
        f.job["settings"]["model"] = json!("claude-opus-5-5");
        f.run().await.unwrap();
        f.job["settings"]["model"] = json!("haiku");
        f.job["settings"]["effort"] = json!("default");
        f.run().await.unwrap();
        let args = f.invocation()["args"].as_array().unwrap().clone();
        assert_eq!(flag(&args, "--model"), Some("haiku"));
        assert!(!args.contains(&json!("--effort")));
    }
    #[tokio::test]
    async fn stale_cached_catalog_does_not_reject_a_moved_alias() {
        let mut f = Fixture::new(json!({}));
        discover(&f.job["settings"], f.root.path()).await.unwrap();
        // The cached catalog predates `opus` moving to the model Claude Code now runs.
        let cache = f.root.path().join("model-catalog-claude.json");
        let mut catalog = crate::util::read_json(&cache).unwrap().unwrap();
        catalog["models"][0]["resolvedModel"] = json!("claude-opus-4-0");
        crate::util::atomic(&cache, &catalog).unwrap();
        f.set(json!({"offline":true}));
        f.run().await.unwrap();
        // A pinned model is still checked against what the session reports.
        f.job["settings"]["model"] = json!("claude-opus-4-0");
        f.set(json!({"offline":true,"wrongModel":true}));
        assert_eq!(kind(f.run().await.unwrap_err()), "config");
    }
    #[tokio::test]
    async fn usage_limits_pause_until_the_reported_reset() {
        let f = Fixture::new(json!({"rateLimited":true}));
        let error = classify_error(&f.run().await.unwrap_err());
        assert_eq!(error.kind, "quota");
        assert!(error.retry_after > 3_000_000, "{}", error.retry_after);
        // Brief throttling without a rejected usage limit is retried automatically.
        let f = Fixture::new(json!({"throttled":true}));
        assert_eq!(kind(f.run().await.unwrap_err()), "transient");
        let f = Fixture::new(json!({"fail":"Overloaded; Retry-After: 45"}));
        let error = classify_error(&f.run().await.unwrap_err());
        assert_eq!(error.kind, "transient");
        assert_eq!(error.retry_after, 45000);
    }
    #[tokio::test]
    async fn cancellation_preserves_the_started_session() {
        let f = Fixture::new(json!({"hang":true}));
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let callbacks = Callbacks {
            on_session: Some(Arc::new(move |_| {
                let stop = stop.clone();
                Box::pin(async move {
                    stop.cancel();
                    Ok(())
                })
            })),
            on_progress: None,
        };
        let error = run_review(
            &f.job,
            &f.source,
            &json!({"files":[]}),
            f.root.path(),
            callbacks,
            cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(kind(error), "interrupted");
        assert_eq!(
            crate::util::read_json(&f.root.path().join("reviews/job-one/session.json"))
                .unwrap()
                .unwrap()["id"],
            "11111111-2222-4333-8444-555555555555"
        );
    }
    #[test]
    fn personal_claude_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let personal = user_home().unwrap().join(".claude");
        let settings = json!({"provider":"claude","claudeHome":personal});
        assert!(environment(&settings, root.path()).is_err());
    }
}
