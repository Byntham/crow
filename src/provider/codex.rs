//! Codex reviews through `codex exec`. Model metadata and effective-policy
//! checks use `codex app-server`. Crow runs Codex with a dedicated
//! `CODEX_HOME`, disables every host tool and native delegation, and verifies
//! the effective configuration before each new or resumed review.
use super::{
    BOUNDARY, Check, Command, Environment, Event, Layout, classify_error, executable, failure,
    isolated_environment, text, user_home, valid_id,
};
use crate::{
    process::{self, RunOptions},
    util,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

const DISABLED: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "apps",
    "plugins",
    "hooks",
    "view_image",
    "image_generation",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "computer_use",
    "in_app_browser",
    "in_app_local_automation",
    "artifact",
    "code_mode_only",
    "memories",
    "skill_search",
    "skill_mcp_dependency_install",
    "tool_suggest",
    "request_permissions_tool",
    "worktrees",
    "goals",
    "shell_snapshot",
    "remote_plugin",
    "recommended_plugins",
    "unbounded_connection_retries",
];
const ESSENTIAL: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "apps",
    "plugins",
    "hooks",
    "view_image",
    "skip_host_skill_discovery",
    "multi_agent",
    "code_mode_host",
    "code_mode",
];

fn program(settings: &Value) -> &str {
    executable(settings, "codex", "codex")
}

/// Resolve Crow's Codex environment. An operator's personal account proxy is
/// inherited when `~/.codex/config.toml` routes Codex through one; its bearer
/// token reaches Codex only through an environment variable.
pub(super) fn environment(settings: &Value, root: &Path) -> Result<Environment> {
    let mut settings = settings.clone();
    let home = user_home()?;
    let inherited = settings["codexProxy"]["configFile"]
        .as_str()
        .map(PathBuf::from);
    let proxy_file = inherited.clone().unwrap_or_else(|| {
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml")
    });
    if inherited.is_some() || std::env::var("CROW_DISABLE_USER_PROXY").ok().as_deref() != Some("1")
    {
        let content = match std::fs::read_to_string(&proxy_file) {
            Ok(s) => s,
            Err(e) if inherited.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
                String::new()
            }
            Err(_) => {
                return Err(failure(
                    "config",
                    "Cannot read the Codex routing configuration.",
                ));
            }
        };
        let config: toml::Value = content
            .parse()
            .map_err(|_| failure("config", "Cannot parse the Codex routing configuration."))?;
        let provider = config
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("openai");
        if provider != "openai" {
            let section = config.get("model_providers").and_then(|p| p.get(provider));
            let get = |name| {
                section
                    .and_then(|v| v.get(name))
                    .and_then(toml::Value::as_str)
            };
            let base_url = get("base_url")
                .ok_or_else(|| failure("config", "The selected Codex provider has no base_url."))?;
            if get("wire_api").is_some_and(|v| v != "responses") {
                return Err(failure(
                    "config",
                    "Crow requires a Responses-compatible Codex provider.",
                ));
            }
            let (token, route) = proxy_token(
                &proxy_file,
                base_url,
                get("env_key"),
                get("experimental_bearer_token"),
                inherited.is_some(),
                |key| std::env::var(key).ok(),
            )?;
            settings["codexProxy"] = json!({"baseUrl":base_url,"configFile":proxy_file});
            return environment_paths(settings, root, &home, Some((token, route)));
        } else if inherited.is_some() {
            return Err(failure(
                "config",
                "The inherited Codex proxy is no longer configured.",
            ));
        }
    }
    // A previously configured route cannot silently reuse stale credentials.
    if settings["codexProxy"].is_object() && inherited.is_none() {
        return Err(failure(
            "config",
            "A Codex proxy requires its routing configuration file.",
        ));
    }
    environment_paths(settings, root, &home, None)
}
// Delegated helpers receive only the normalized credential. Tie it to the exact
// reread route and environment key so editing routing cannot forward a stale token.
fn proxy_token(
    config_file: &Path,
    base_url: &str,
    env_key: Option<&str>,
    literal: Option<&str>,
    inherited: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(String, String)> {
    let canonical = std::fs::canonicalize(config_file).unwrap_or(config_file.to_owned());
    let route = util::hash(&json!({"configFile":canonical,"baseUrl":base_url,"envKey":env_key}));
    let token = literal
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| env_key.and_then(&lookup).filter(|s| !s.is_empty()))
        .or_else(|| {
            if inherited
                && env_key.is_some()
                && lookup("CROW_CODEX_PROXY_ROUTE").as_deref() == Some(route.as_str())
            {
                lookup("CROW_CODEX_PROXY_TOKEN").filter(|s| !s.is_empty())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            failure(
                "auth",
                "The selected Codex proxy has no available bearer token.",
            )
        })?;
    Ok((token, route))
}
fn environment_paths(
    settings: Value,
    root: &Path,
    home: &Path,
    token: Option<(String, String)>,
) -> Result<Environment> {
    let (codex_home, mut env) = isolated_environment(
        root,
        "codexHome",
        "CODEX_HOME",
        "codex",
        &home.join(".codex"),
        &settings,
    )?;
    for name in ["AGENTS.md", "AGENTS.override.md"] {
        if codex_home.join(name).exists() {
            return Err(failure(
                "config",
                format!(
                    "Remove {name} from Crow's dedicated Codex directory. Set review guidance through Crow instead."
                ),
            ));
        }
    }
    if std::fs::read_dir(codex_home.join("agents"))
        .ok()
        .is_some_and(|mut d| d.next().is_some())
    {
        return Err(failure(
            "config",
            "Crow Codex directory contains custom agents that could override review controls. Use a clean Crow state directory.",
        ));
    }
    if let Some((token, route)) = token {
        env.insert("CROW_CODEX_PROXY_TOKEN".into(), token);
        env.insert("CROW_CODEX_PROXY_ROUTE".into(), route);
    }
    Ok(Environment { env, settings })
}
fn transport_config(settings: &Value) -> Map<String, Value> {
    let proxy = &settings["codexProxy"];
    let config = if proxy.is_object() {
        json!({
            "model_provider": "crow_proxy",
            "model_providers.crow_proxy.name": "Crow account proxy",
            "model_providers.crow_proxy.base_url": proxy["baseUrl"],
            "model_providers.crow_proxy.wire_api": "responses",
            "model_providers.crow_proxy.requires_openai_auth": false,
            "model_providers.crow_proxy.supports_websockets": false,
            "model_providers.crow_proxy.env_key": "CROW_CODEX_PROXY_TOKEN",
        })
    } else {
        json!({"model_provider":"openai","forced_login_method":"chatgpt"})
    };
    config.as_object().unwrap().clone()
}
fn base_config(settings: &Value) -> Map<String, Value> {
    let mut c = transport_config(settings);
    let controls = json!({
        "cli_auth_credentials_store": "file",
        "approval_policy": "never",
        "sandbox_mode": "read-only",
        "web_search": "disabled",
        "project_doc_max_bytes": 0,
        "features.skip_host_skill_discovery": true,
        // Some models need the Code Mode host to call tools at all. Excluding the
        // `functions` namespace removes the hidden edit and host execution tools.
        "features.code_mode_host": true,
        "features.code_mode.enabled": true,
        "features.code_mode.excluded_tool_namespaces": ["functions"],
    });
    c.extend(controls.as_object().unwrap().clone());
    for name in DISABLED {
        c.insert(format!("features.{name}"), json!(false));
    }
    c
}
fn toml_literal(v: &Value) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(toml_literal).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}={}", json!(k), toml_literal(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => v.to_string(),
    }
}
fn config_args(c: &Map<String, Value>) -> Vec<String> {
    c.iter()
        .flat_map(|(k, v)| vec!["-c".into(), format!("{k}={}", toml_literal(v))])
        .collect()
}

/// A short-lived `codex app-server` JSON-RPC connection for metadata.
struct Rpc {
    child: Child,
    pid: u32,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    reader: tokio::task::JoinHandle<()>,
    sequence: u64,
    bytes: usize,
    deadline: Instant,
    detached: bool,
}
impl Drop for Rpc {
    fn drop(&mut self) {
        if let Some(pid) = self
            .child
            .id()
            .or_else(|| self.detached.then_some(self.pid))
        {
            #[cfg(unix)]
            unsafe {
                libc::kill(
                    if self.detached {
                        -(pid as i32)
                    } else {
                        pid as i32
                    },
                    libc::SIGKILL,
                );
            }
        }
        self.reader.abort();
    }
}
impl Rpc {
    async fn start(
        settings: &Value,
        root: &Path,
        cwd: Option<&Path>,
        extra: Option<&Map<String, Value>>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let environment = environment(settings, root)?;
        let mut config = base_config(&environment.settings);
        if let Some(extra) = extra {
            config.extend(extra.clone());
        }
        let mut command = tokio::process::Command::new(program(settings));
        command
            .args(config_args(&config))
            .args(["app-server", "--strict-config"])
            .env_clear()
            .envs(&environment.env)
            .current_dir(cwd.unwrap_or_else(|| Path::new(&environment.env["HOME"])))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let detached = settings["detached"] != false;
        #[cfg(unix)]
        if detached {
            command.process_group(0);
        }
        if cancel.is_cancelled() {
            return Err(failure("interrupted", "Interrupted"));
        }
        let mut child = command
            .spawn()
            .context("Cannot start Codex metadata connection")?;
        let pid = child.id().context("Codex metadata process has no ID")?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut error = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let ring = stderr.clone();
        let reader = tokio::spawn(async move {
            let mut buf = [0; 4096];
            loop {
                match error.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut ring = ring.lock().unwrap();
                        ring.extend_from_slice(&buf[..n]);
                        let extra = ring.len().saturating_sub(4000);
                        ring.drain(..extra);
                    }
                }
            }
        });
        let mut rpc = Self {
            child,
            pid,
            input,
            output,
            stderr,
            reader,
            sequence: 0,
            bytes: 0,
            deadline: Instant::now() + Duration::from_secs(30),
            detached,
        };
        let client = json!({"name":"crow","title":"Crow","version":env!("CARGO_PKG_VERSION")});
        rpc.request("initialize", json!({"clientInfo":client}), cancel)
            .await?;
        rpc.send(json!({"method":"initialized","params":{}}))
            .await?;
        Ok(rpc)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let buf = self.output.fill_buf().await?;
            if buf.is_empty() {
                if !line.is_empty() {
                    return Ok(line);
                }
                // Classify failures only after stderr is drained; an EOF race must
                // not turn an auth/config failure into a cached-catalog fallback.
                let status = self.child.wait().await?;
                #[cfg(unix)]
                if self.detached {
                    unsafe {
                        libc::kill(-(self.pid as i32), libc::SIGKILL);
                    }
                }
                let _ = (&mut self.reader).await;
                let detail = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
                bail!("Codex metadata connection exited ({status}): {detail}");
            }
            let n = buf
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(buf.len());
            self.bytes += n;
            if self.bytes > 8 * 1024 * 1024 {
                bail!("Codex metadata response exceeds 8 MB");
            }
            let end = buf[n - 1] == b'\n';
            line.extend_from_slice(&buf[..n]);
            self.output.consume(n);
            if end {
                return Ok(line);
            }
        }
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        let result = tokio::time::timeout(timeout, async {
            self.sequence += 1;
            let id = self.sequence;
            self.send(json!({"id":id,"method":method,"params":params}))
                .await?;
            loop {
                let line = self.line().await?;
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let m: Value =
                    serde_json::from_slice(&line).context("Invalid JSON from Codex app-server")?;
                if m["id"] == id {
                    if !m["error"].is_null() {
                        bail!(
                            "{}",
                            m["error"]["message"]
                                .as_str()
                                .unwrap_or("Codex metadata error")
                        );
                    }
                    return Ok(m["result"].clone());
                }
                if !m["id"].is_null() && m["method"].is_string() {
                    let refusal = json!({
                        "id": m["id"],
                        "error": {
                            "code": -32601,
                            "message": "Crow metadata client does not accept interactive requests",
                        },
                    });
                    self.send(refusal).await?;
                }
            }
        });
        tokio::select! {
            _ = cancel.cancelled() => Err(failure("interrupted", "Interrupted")),
            value = result => value.map_err(|_| anyhow!("Codex metadata request timed out"))?,
        }
    }
}

pub(super) async fn auth(
    settings: &Value,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Value> {
    let env = environment(settings, root)?;
    let mut rpc = Rpc::start(&env.settings, root, None, None, cancel).await?;
    if env.settings["codexProxy"].is_object() {
        let probe = rpc
            .request(
                "model/list",
                json!({"limit":1,"includeHidden":false}),
                cancel,
            )
            .await?;
        if !probe["data"].is_array() {
            return Err(failure(
                "auth",
                "The configured Codex proxy did not return a model catalog.",
            ));
        }
        return Ok(json!({"authenticated":true,"account":{"type":"proxy"},"warning":null}));
    }
    let response = rpc
        .request("account/read", json!({"refreshToken":false}), cancel)
        .await?;
    let authenticated = response["account"]["type"] == "chatgpt";
    let warning = (!authenticated).then_some(
        "Sign in to a ChatGPT subscription with crow login. API-key authentication is not supported.",
    );
    Ok(json!({"authenticated":authenticated,"account":response["account"],"warning":warning}))
}

/// Proxy routes keep their own catalog so a cached list never crosses accounts.
pub(super) fn catalog_cache(settings: &Value, root: &Path) -> Result<PathBuf> {
    let environment = environment(settings, root)?;
    let proxy = &environment.settings["codexProxy"];
    Ok(root.join(if proxy.is_object() {
        format!("model-catalog-proxy-{}.json", util::hash(proxy))
    } else {
        "model-catalog.json".into()
    }))
}
pub(super) async fn catalog(
    settings: &Value,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Value> {
    let environment = environment(settings, root)?;
    let proxy = environment.settings["codexProxy"].is_object();
    let mut rpc = Rpc::start(&environment.settings, root, None, None, cancel).await?;
    let account = if proxy {
        json!({"type":"proxy"})
    } else {
        rpc.request("account/read", json!({"refreshToken":false}), cancel)
            .await?["account"]
            .clone()
    };
    if !proxy && account["type"] != "chatgpt" {
        return Err(failure(
            "auth",
            "A ChatGPT subscription login is required for model discovery. Run crow login.",
        ));
    }
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = None;
    loop {
        let mut params = json!({"limit":100,"includeHidden":false});
        if let Some(c) = cursor {
            params["cursor"] = json!(c);
        }
        let page = rpc.request("model/list", params, cancel).await?;
        let data = page["data"]
            .as_array()
            .context("Provider returned an invalid model catalog")?;
        models.extend(data.iter().cloned());
        cursor = page["nextCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        match &cursor {
            Some(c) => {
                if !seen.insert(c.clone()) || seen.len() > 101 {
                    bail!("Provider returned repeated model pages");
                }
            }
            None => break,
        }
    }
    Ok(json!({"models":models,"account":account}))
}

pub(super) async fn login(settings: &Value, root: &Path) -> Result<()> {
    let env = environment(settings, root)?;
    if env.settings["codexProxy"].is_object() {
        return Ok(());
    }
    let mut args = config_args(&base_config(&env.settings));
    args.extend(["login".into(), "--device-auth".into()]);
    process::run(
        program(settings),
        &args,
        RunOptions {
            cwd: Some(PathBuf::from(&env.env["HOME"])),
            env: Some(env.env),
            inherit: true,
            detached: false,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn checks(settings: &Value, root: &Path) -> Result<Vec<Check>> {
    let env = environment(settings, root)?;
    let run = |args: &'static [&'static str]| {
        let env = env.env.clone();
        async move {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            process::run(
                program(settings),
                &args,
                RunOptions {
                    cwd: Some(PathBuf::from(&env["HOME"])),
                    env: Some(env),
                    timeout: Some(Duration::from_secs(10)),
                    ..Default::default()
                },
            )
            .await
            .map(|out| out.stdout)
        }
    };
    let version = run(&["--version"]).await.map(|out| out.trim().to_owned());
    let exec =
        run(&["exec", "resume", "--help"]).await.and_then(|out| {
            for flag in [
                "--json",
                "--output-schema",
                "--output-last-message",
                "--ignore-user-config",
                "--ignore-rules",
            ] {
                if !out.contains(flag) {
                    bail!("Installed Codex lacks {flag}; update the official CLI.");
                }
            }
            Ok("JSON exec/resume, structured output, and user-config/rule isolation are supported."
            .to_owned())
        });
    let features = run(&["features", "list"]).await.and_then(|out| {
        for feature in ESSENTIAL {
            if !out
                .lines()
                .any(|line| line.split_whitespace().next() == Some(feature))
            {
                bail!("Installed Codex lacks {feature}; update the official CLI.");
            }
        }
        Ok(format!(
            "All {} required inspection isolation controls are available.",
            ESSENTIAL.len()
        ))
    });
    Ok(vec![
        Check {
            name: "Official Codex executable",
            result: version,
        },
        Check {
            name: "Persistent JSON exec and resume",
            result: exec,
        },
        Check {
            name: "Inspection isolation controls",
            result: features,
        },
    ])
}

/// Disable every skill under Crow's Codex home; skills could carry instructions
/// or tools outside the review boundary.
fn skill_disables(path: &Path, depth: usize, out: &mut Vec<Value>) -> Result<()> {
    if depth > 5 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            skill_disables(&entry.path(), depth + 1, out)?;
        } else if kind.is_file() && entry.file_name() == "SKILL.md" {
            out.push(json!({"path":entry.path(),"enabled":false}));
        }
    }
    Ok(())
}

pub(super) struct Review {
    config: Map<String, Value>,
    env: BTreeMap<String, String>,
    settings: Value,
}
impl Review {
    pub(super) async fn prepare(
        layout: &Layout,
        environment: Environment,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let settings = environment.settings;
        let mut config = base_config(&settings);
        let helper = std::env::current_exe()?;
        let review = json!({
            "model": settings["model"],
            "model_reasoning_effort": settings["effort"],
            "developer_instructions": BOUNDARY,
            // Crow's MCP delegation tools replace native subagents, whose
            // custom roles can override the configured child model.
            "features.multi_agent": false,
            "features.multi_agent_v2": false,
            "agents.enabled": false,
            "agents.max_concurrent_threads_per_session": layout.max_subagents.max(1),
            "agents.default_subagent_model": layout.subagent["model"],
            "agents.default_subagent_reasoning_effort": layout.subagent["effort"],
            "mcp_servers.crow_inspection.command": helper,
            "mcp_servers.crow_inspection.args": ["_inspection-mcp", layout.source_path, layout.context_path],
            "mcp_servers.crow_inspection.required": true,
            "mcp_servers.crow_inspection.default_tools_approval_mode": "approve",
            "mcp_servers.crow_inspection.enabled_tools": layout.tools,
            "mcp_servers.crow_inspection.startup_timeout_sec": 20,
            // Codex filters inherited MCP environments. Explicitly forward only the
            // normalized credential and its route proof; never put secrets in config argv.
            "mcp_servers.crow_inspection.env_vars": if settings["codexProxy"].is_object() {
                json!(["CROW_CODEX_PROXY_TOKEN", "CROW_CODEX_PROXY_ROUTE"])
            } else {
                json!([])
            },
        });
        config.extend(review.as_object().unwrap().clone());
        let mut skills = Vec::new();
        skill_disables(
            &Path::new(&environment.env["CODEX_HOME"]).join("skills"),
            0,
            &mut skills,
        )?;
        if !skills.is_empty() {
            config.insert("skills.config".into(), json!(skills));
        }
        let review = Self {
            config,
            env: environment.env,
            settings,
        };
        review.verify_policy(layout, root, cancel).await?;
        Ok(review)
    }
    /// Ask Codex for its effective configuration, including host-managed
    /// requirements, and refuse to start unless every Crow control applied.
    async fn verify_policy(
        &self,
        layout: &Layout,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut rpc = Rpc::start(
            &self.settings,
            root,
            Some(&layout.cwd),
            Some(&self.config),
            cancel,
        )
        .await?;
        let result = rpc
            .request("config/read", json!({"includeLayers":false}), cancel)
            .await?;
        let c = &result["config"];
        let features = &c["features"];
        if c["sandbox_mode"] != "read-only"
            || c["approval_policy"] != "never"
            || (!self.settings["codexProxy"].is_object() && c["forced_login_method"] != "chatgpt")
        {
            return Err(failure(
                "config",
                "Codex could not apply Crow subscription and inspection permissions. Check host-managed Codex requirements.",
            ));
        }
        for (key, expected) in transport_config(&self.settings) {
            let actual = key.split('.').fold(c, |value, part| &value[part]);
            if actual != &expected {
                return Err(failure(
                    "config",
                    "Codex did not apply the configured Crow transport.",
                ));
            }
        }
        for name in DISABLED {
            if features[name] != false {
                return Err(failure(
                    "config",
                    format!(
                        "Codex did not disable {name}. Update or repair the official runtime before reviewing."
                    ),
                ));
            }
        }
        if features["multi_agent"] != false || features["multi_agent_v2"] != false {
            return Err(failure(
                "config",
                "Codex native delegation must be disabled.",
            ));
        }
        if !features["code_mode"]["excluded_tool_namespaces"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("functions")))
        {
            return Err(failure(
                "config",
                "Codex did not exclude host tools from its orchestration runtime.",
            ));
        }
        if c["model"] != self.settings["model"]
            || c["model_reasoning_effort"] != self.settings["effort"]
        {
            return Err(failure(
                "config",
                "Host-managed Codex settings changed the requested review model or reasoning.",
            ));
        }
        let servers = c["mcp_servers"]
            .as_object()
            .context("Codex did not expose its MCP configuration")?
            .iter()
            .filter(|(_, s)| s["enabled"] != false)
            .collect::<Vec<_>>();
        if servers.len() != 1 || servers[0].0 != "crow_inspection" {
            return Err(failure(
                "config",
                "Unexpected MCP servers are configured outside Crow. Remove them from Crow or host-wide Codex settings.",
            ));
        }
        let inspection = servers[0].1;
        for key in [
            "command",
            "args",
            "required",
            "default_tools_approval_mode",
            "enabled_tools",
            "env_vars",
        ] {
            if inspection[key] != self.config[&format!("mcp_servers.crow_inspection.{key}")] {
                return Err(failure(
                    "config",
                    "Codex did not apply the controlled Crow inspection helper.",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn command(&self, layout: &Layout, session: Option<&str>) -> Command {
        let mut args = config_args(&self.config);
        args.push("exec".into());
        if let Some(session) = session {
            args.extend(["resume".into(), session.into()]);
        }
        for arg in [
            "--ignore-user-config",
            "--ignore-rules",
            "--strict-config",
            "--skip-git-repo-check",
            "--json",
            "--output-schema",
        ] {
            args.push(arg.into());
        }
        args.push(layout.schema_path.to_string_lossy().into_owned());
        args.push("--output-last-message".into());
        args.push(layout.output_path.to_string_lossy().into_owned());
        args.push("-".into());
        Command {
            program: program(&self.settings).into(),
            args,
            env: self.env.clone(),
        }
    }
}

pub(super) fn parse(event: &Value) -> Result<Vec<Event>> {
    Ok(match text(event, "type") {
        "turn.completed" => vec![Event::Completed],
        "thread.started" => {
            let id = event["thread_id"]
                .as_str()
                .filter(|s| valid_id(s))
                .ok_or_else(|| failure("restart", "Codex did not provide a usable session ID."))?;
            vec![Event::Session(id.into())]
        }
        "error" | "turn.failed" => {
            let message = event["error"]["message"]
                .as_str()
                .or_else(|| event["message"].as_str())
                .unwrap_or("Codex review failed");
            vec![Event::Failed(classify_error(&anyhow!(message.to_owned())))]
        }
        "item.completed" => {
            let item = &event["item"];
            let mut events = Vec::new();
            let kind = match text(item, "type") {
                "agent_message" => {
                    if let Some(text) = item["text"].as_str() {
                        events.push(Event::Message(text.into()));
                    }
                    Some("agent_message")
                }
                "mcp_tool_call" => Some("mcp_tool_call"),
                "collab_tool_call" => Some("collab_tool_call"),
                "reasoning" => Some("reasoning"),
                _ => None,
            };
            if let Some(kind) = kind
                && item["status"] != "failed"
            {
                events.push(Event::Progress(kind));
            }
            events
        }
        _ => vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Callbacks, diagnostics, discover, run_review};
    use super::*;
    fn compiled_fake() -> PathBuf {
        super::super::fixtures::compile("codex", include_str!("../../tests/fixtures/fake_codex.rs"))
    }
    struct Fixture {
        root: tempfile::TempDir,
        job: Value,
        source: Value,
    }
    impl Fixture {
        fn new(behavior: Value) -> Self {
            let root = tempfile::tempdir().unwrap();
            let codex = root.path().join("fake-codex");
            // Reuse the immutable executable without opening another copy for
            // writing while the parallel suite launches fixture processes.
            std::fs::hard_link(compiled_fake(), &codex).unwrap();
            util::atomic(&root.path().join("behavior.json"), &behavior).unwrap();
            let source = json!({
                "dir": root.path().join("source"),
                "head": "a".repeat(40),
                "base": "b".repeat(40),
                "target": "main",
                "targetSha": "c".repeat(40),
            });
            // Explicit local config avoids depending on the developer's personal provider routing.
            let config = root.path().join("proxy.toml");
            std::fs::write(
                &config,
                "model_provider = 'fixture'\n[model_providers.fixture]\nbase_url = 'http://localhost:1'\nwire_api = 'responses'\nexperimental_bearer_token = 'fixture-token'\n",
            )
            .unwrap();
            let job = json!({
                "id": "job-one",
                "repo": "owner/repo",
                "number": 1,
                "comparison": source,
                "settings": {
                    "codex": codex,
                    "codexHome": root.path().join("codex"),
                    "codexProxy": {"configFile": config, "baseUrl": "http://localhost:1"},
                    "model": "provider-default",
                    "effort": "medium",
                    "subagents": {"mode": "inherit", "max": 8},
                    "retry": {"mode": "fixed", "count": 10, "delayMs": 5000},
                },
            });
            Self { root, job, source }
        }
        fn set(&self, behavior: Value) {
            util::atomic(&self.root.path().join("behavior.json"), &behavior).unwrap();
        }
        async fn run(&self) -> Result<Value> {
            run_review(
                &self.job,
                &self.source,
                &json!({"files":[]}),
                self.root.path(),
                Callbacks::default(),
                CancellationToken::new(),
            )
            .await
        }
    }
    #[tokio::test]
    async fn successful_diagnostics_summarize_capabilities_and_preserve_warnings() {
        let f = Fixture::new(json!({}));
        let result = diagnostics(&f.job["settings"], f.root.path(), true)
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["version"], "codex-cli 0.154.0");
        assert_eq!(result["checks"].as_array().unwrap().len(), 4);
        let output = result.to_string();
        assert!(!output.contains("--output-schema"));
        assert!(!output.contains("stable true"));
        assert!(output.contains("JSON exec/resume"));
        assert!(output.contains("10 required inspection isolation controls"));
        assert!(output.contains("2 models available"));
        assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
        f.set(json!({"offline":true}));
        let cached = diagnostics(&f.job["settings"], f.root.path(), true)
            .await
            .unwrap();
        assert_eq!(cached["ok"], true);
        assert!(
            cached["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("cached list"))
        );
        let unavailable = Fixture::new(json!({"offline":true}));
        let failed = diagnostics(&unavailable.job["settings"], unavailable.root.path(), true)
            .await
            .unwrap();
        assert_eq!(failed["ok"], false);
        assert!(
            failed["checks"][3]["detail"]
                .as_str()
                .unwrap()
                .contains("no cached list")
        );
    }
    #[tokio::test]
    async fn discovery_paginates_and_falls_back_without_inventing_models() {
        let f = Fixture::new(json!({}));
        let catalog = discover(&f.job["settings"], f.root.path()).await.unwrap();
        assert_eq!(catalog["models"].as_array().unwrap().len(), 2);
        assert_eq!(catalog["cached"], false);
        f.set(json!({"offline":true}));
        let old = discover(&f.job["settings"], f.root.path()).await.unwrap();
        assert_eq!(old["cached"], true);
        assert_eq!(old["models"], catalog["models"]);
        let fresh = Fixture::new(json!({"offline":true}));
        assert!(
            discover(&fresh.job["settings"], fresh.root.path())
                .await
                .unwrap_err()
                .to_string()
                .contains("no cached list")
        );
    }
    #[tokio::test]
    async fn controlled_review_persists_session_before_callback_and_report() {
        let f = Fixture::new(json!({}));
        let file = f.root.path().join("reviews/job-one/session.json");
        let called = Arc::new(Mutex::new(Vec::new()));
        let sessions = called.clone();
        let callbacks = Callbacks {
            on_session: Some(Arc::new(move |id| {
                let file = file.clone();
                let sessions = sessions.clone();
                Box::pin(async move {
                    assert_eq!(util::read_json(&file)?.unwrap()["id"], id);
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
        assert_eq!(*called.lock().unwrap(), vec![json!("saved-session")]);
        let invocation = util::read_json(&f.root.path().join("invocation.json"))
            .unwrap()
            .unwrap();
        assert_ne!(invocation["cwd"], f.source["dir"]);
        assert_eq!(invocation["env"]["OPENAI_API_KEY"], Value::Null);
        assert_eq!(invocation["env"]["CROW_CODEX_PROXY_TOKEN"], "fixture-token");
        assert!(text(&invocation, "prompt").contains("Target guidance"));
        let context = util::read_json(
            &f.root
                .path()
                .join("reviews/job-one/delegation-context.json"),
        )
        .unwrap()
        .unwrap();
        assert!(!context.to_string().contains("fixture-token"));
        let args = invocation["args"].as_array().unwrap();
        assert!(args.contains(&json!("mcp_servers.crow_inspection.env_vars=[\"CROW_CODEX_PROXY_TOKEN\",\"CROW_CODEX_PROXY_ROUTE\"]")));
        assert!(
            !serde_json::to_string(args)
                .unwrap()
                .contains("fixture-token")
        );

        assert_eq!(
            util::read_json(&f.root.path().join("reviews/job-one/report.json"))
                .unwrap()
                .unwrap(),
            report
        );
    }
    #[tokio::test]
    async fn invalid_output_preserves_session_and_resume_rejects_replacement() {
        let mut f = Fixture::new(json!({"invalid":true}));
        assert_eq!(classify_error(&f.run().await.unwrap_err()).kind, "output");
        f.set(json!({}));
        f.job["session"] = json!("saved-session");
        f.run().await.unwrap();
        let invocation = util::read_json(&f.root.path().join("invocation.json"))
            .unwrap()
            .unwrap();
        assert!(
            invocation["args"]
                .as_array()
                .unwrap()
                .contains(&json!("resume"))
        );
        f.set(json!({"wrongSession":true}));
        assert_eq!(classify_error(&f.run().await.unwrap_err()).kind, "restart");
    }
    #[tokio::test]
    async fn policy_and_model_failures_prevent_inference() {
        for behavior in [
            json!({"unsafe":true}),
            json!({"wrongModel":true}),
            json!({"missingProxyEnv":true}),
        ] {
            let f = Fixture::new(behavior);
            assert_eq!(classify_error(&f.run().await.unwrap_err()).kind, "config");
            assert!(!f.root.path().join("invocation.json").exists());
        }
        let mut f = Fixture::new(json!({}));
        f.job["settings"]["effort"] = json!("unsupported");
        assert_eq!(classify_error(&f.run().await.unwrap_err()).kind, "config");
        assert!(!f.root.path().join("invocation.json").exists());
    }
    #[tokio::test]
    async fn provider_failure_keeps_classification_and_retry_delay() {
        let f = Fixture::new(json!({"fail":"service unavailable; Retry-After: 45"}));
        let error = classify_error(&f.run().await.unwrap_err());
        assert_eq!(error.kind, "transient");
        assert_eq!(error.retry_after, 45000);
    }
    #[tokio::test]
    async fn incomplete_delegation_blocks_complete_parent_report() {
        let f = Fixture::new(json!({}));
        util::atomic(
            &f.root.path().join("reviews/job-one/tasks/abcd.json"),
            &json!({"id":"abcd","state":"paused"}),
        )
        .unwrap();
        assert_eq!(classify_error(&f.run().await.unwrap_err()).kind, "output");
        assert!(!f.root.path().join("reviews/job-one/report.json").exists());
        util::atomic(
            &f.root.path().join("reviews/job-one/tasks/abcd.json"),
            &json!({"id":"abcd","state":"completed"}),
        )
        .unwrap();
        f.run().await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_preserves_the_latest_provider_session() {
        let f = Fixture::new(json!({"hang":true}));
        let cancel = CancellationToken::new();
        let session_cancel = cancel.clone();
        let callbacks = Callbacks {
            on_session: Some(Arc::new(move |_| {
                let cancel = session_cancel.clone();
                Box::pin(async move {
                    cancel.cancel();
                    Ok(())
                })
            })),
            on_progress: None,
        };
        let result = run_review(
            &f.job,
            &f.source,
            &json!({"files":[]}),
            f.root.path(),
            callbacks,
            cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(classify_error(&result).kind, "interrupted");
        assert_eq!(
            util::read_json(&f.root.path().join("reviews/job-one/session.json"))
                .unwrap()
                .unwrap()["id"],
            "saved-session"
        );
    }
    #[test]
    fn inherited_environment_proxy_token_is_bound_to_its_current_route() {
        let file = Path::new("/fixture/config.toml");
        let (token, route) = proxy_token(
            file,
            "https://proxy.test",
            Some("PRIVATE_KEY"),
            None,
            false,
            |k| (k == "PRIVATE_KEY").then(|| "secret".into()),
        )
        .unwrap();
        let normalized = BTreeMap::from([
            ("CROW_CODEX_PROXY_TOKEN".to_owned(), token),
            ("CROW_CODEX_PROXY_ROUTE".to_owned(), route),
        ]);
        assert_eq!(
            proxy_token(
                file,
                "https://proxy.test",
                Some("PRIVATE_KEY"),
                None,
                true,
                |k| normalized.get(k).cloned()
            )
            .unwrap()
            .0,
            "secret"
        );
        for (url, key) in [
            ("https://changed.test", "PRIVATE_KEY"),
            ("https://proxy.test", "OTHER_KEY"),
        ] {
            assert!(
                proxy_token(file, url, Some(key), None, true, |k| normalized
                    .get(k)
                    .cloned())
                .is_err()
            );
        }
        assert!(
            proxy_token(
                file,
                "https://proxy.test",
                Some("PRIVATE_KEY"),
                None,
                false,
                |k| normalized.get(k).cloned()
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn metadata_stderr_auth_error_never_uses_cached_catalog() {
        let f = Fixture::new(json!({}));
        discover(&f.job["settings"], f.root.path()).await.unwrap();
        f.set(json!({"metadataExit":"401 unauthorized"}));
        for _ in 0..10 {
            assert_eq!(
                classify_error(
                    &discover(&f.job["settings"], f.root.path())
                        .await
                        .unwrap_err()
                )
                .kind,
                "auth"
            );
        }
    }
    #[test]
    fn proxy_toml_uses_real_parser_and_isolated_home() {
        let f = Fixture::new(json!({}));
        let config = PathBuf::from(text(&f.job["settings"]["codexProxy"], "configFile"));
        std::fs::write(&config,"model_provider = 'custom.name'\n[model_providers.'custom.name']\nbase_url = \"http://localhost:1\"\nexperimental_bearer_token = 'secret'\n").unwrap();
        let env = environment(&f.job["settings"], f.root.path()).unwrap();
        assert_eq!(env.env["CROW_CODEX_PROXY_TOKEN"], "secret");
        assert_ne!(env.env["HOME"], std::env::var("HOME").unwrap());
    }
}
