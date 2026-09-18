//! User-facing commands. Clap validates command shape before any side effects.
use crate::{backup, config, inspection, operations, provider, setup, util};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Readable summaries and tables.
    #[default]
    Text,
    /// Structured command results for scripts.
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "crow",
    about = "Review GitHub pull requests with your own Crow service",
    disable_version_flag = true,
    help_template = root_help_template(),
    before_help = "Get started:
  crow setup                    Set up this machine
  crow enroll owner/repo        Start reviewing a repository
  crow status                   See reviews and worker health",
    after_help = "Use crow <command> --help for details and examples.
For scripts: crow status --format json
CROW_HOME sets the installation directory (default: ~/.local/share/crow)."
)]
struct Cli {
    /// Show the installed Crow version.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    version: bool,
    /// Result format; JSON is available for noninteractive commands.
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: OutputFormat,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show the installed Crow version.
    Version,
    /// Install Crow on this machine and start setup.
    Install {
        /// Install without opening the setup wizard.
        #[arg(long)]
        no_setup: bool,
    },
    /// Set up GitHub access, a review worker, and the service.
    #[command(after_help = "Examples:
  crow setup
  crow setup --role worker

The service receives GitHub events. Workers run reviews. Choose both for one machine.")]
    Setup {
        /// What this machine runs: both, the service, or a worker.
        #[arg(long, value_parser = ["both", "service", "worker"])]
        role: Option<String>,
        /// How GitHub reaches the service over HTTPS.
        #[arg(long, value_parser = ["funnel", "cloudflare", "existing"])]
        ingress: Option<String>,
        /// Local port for the service.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Run Crow in this terminal until you press Ctrl-C.
    Run,
    /// Start Crow in the background.
    Start,
    /// Stop Crow's background service.
    Stop,
    /// Restart the service after configuration changes.
    ServiceRestart,
    /// Show repositories, workers, and recent reviews.
    Status,
    /// Check the installation and explain any problems.
    Doctor {
        /// Also check Codex capabilities without running a review.
        #[arg(long)]
        runtime: bool,
    },
    /// Follow service logs; press Ctrl-C to stop watching.
    Logs,
    /// Sign in to the review provider on this worker.
    Login,
    /// List available review models and reasoning levels.
    Models,
    /// Enable automatic reviews for a GitHub repository.
    #[command(after_help = "Example: crow enroll owner/repo

By default, reviews cover new pull requests by your GitHub account.
Use crow policy to change who receives or requests reviews.")]
    Enroll {
        /// Repository in OWNER/REPO format.
        repo: String,
        /// Also review eligible pull requests that are already open.
        #[arg(long)]
        include_backlog: bool,
        /// Refresh the GitHub App installation for an enrolled repository.
        #[arg(long)]
        reenroll: bool,
        /// Paired worker ID; required on a service-only machine.
        #[arg(long)]
        worker: Option<String>,
    },
    /// Choose whose PRs get reviews and who can request them.
    #[command(group(clap::ArgGroup::new("policy_change").args(["authors", "everyone", "requesters"]).required(true).multiple(true)), after_help = "Examples:
  crow policy owner/repo --authors alice,bob
  crow policy owner/repo --everyone
  crow policy owner/repo --requesters alice,bob")]
    Policy {
        /// Repository in OWNER/REPO format.
        repo: String,
        /// Review pull requests from these comma-separated GitHub usernames.
        #[arg(long, conflicts_with = "everyone", value_name = "USER,USER")]
        authors: Option<String>,
        /// Review pull requests from all authors.
        #[arg(long)]
        everyone: bool,
        /// Allow these comma-separated GitHub usernames to request reviews.
        #[arg(long, value_name = "USER,USER")]
        requesters: Option<String>,
    },
    /// Set review options for one repository.
    #[command(group(clap::ArgGroup::new("settings").args(["json", "model", "effort", "timeout_seconds", "reset"]).required(true).multiple(true)), after_help = "Examples:
  crow repo-config owner/repo --model MODEL --effort high
  crow repo-config owner/repo --reset
  crow repo-config owner/repo --json '{\"retry\":{\"count\":3}}'

Choose MODEL from crow models. Each call replaces this repository's overrides.
Omitted options use worker defaults.")]
    RepoConfig {
        /// Repository in OWNER/REPO format.
        repo: String,
        /// Advanced: review overrides as a JSON object.
        #[arg(long, conflicts_with_all = ["model", "effort", "timeout_seconds", "reset"], value_name = "OBJECT")]
        json: Option<String>,
        /// Model to use for this repository; see crow models.
        #[arg(long, conflicts_with = "reset")]
        model: Option<String>,
        /// Reasoning level for this repository; see crow models.
        #[arg(long, conflicts_with = "reset")]
        effort: Option<String>,
        /// Review time limit in seconds; 0 means no limit.
        #[arg(long, conflicts_with = "reset", value_parser = clap::value_parser!(u64).range(0..=604800))]
        timeout_seconds: Option<u64>,
        /// Remove repository overrides and use the worker defaults.
        #[arg(long)]
        reset: bool,
    },
    /// Request a review of a pull request.
    #[command(after_help = "Example: crow review owner/repo 42")]
    Review(ReviewArgs),
    /// Pause a review and keep its saved work.
    #[command(after_help = "Example: crow pause owner/repo 42
Continue later with crow resume owner/repo 42.")]
    Pause(ReviewArgs),
    /// Continue a paused review from its saved work.
    #[command(after_help = "Example: crow resume owner/repo 42")]
    Resume {
        #[command(flatten)]
        review: ReviewArgs,
        /// Change the review model; see crow models.
        #[arg(long)]
        model: Option<String>,
        /// Change the reasoning level; see crow models.
        #[arg(long)]
        effort: Option<String>,
    },
    /// Restart a pull request review from scratch.
    #[command(after_help = "Example: crow restart owner/repo 42")]
    Restart(ReviewArgs),
    /// Find eligible open pull requests that need a review.
    CatchUp {
        /// Limit to OWNER/REPO; omit to check all enrolled repositories.
        repo: Option<String>,
        /// Also include pull requests opened before enrollment.
        #[arg(long)]
        include_backlog: bool,
    },
    /// Queue reviews held back after a large catch-up batch.
    Release {
        /// Limit to OWNER/REPO; omit to release all held reviews.
        repo: Option<String>,
    },
    /// Create credentials for connecting another worker.
    Pair,
    /// Show settings, or change a setting on this machine.
    #[command(after_help = "Examples:
  crow config
  crow config worker.model MODEL
  crow config worker.effort high
  crow config worker.concurrency 2
  crow config catchUp.enabled true

Editable settings:
  worker.model, worker.effort, worker.concurrency, worker.timeoutMs
  worker.subagents, worker.retry, worker.execution (JSON objects)
  catchUp.enabled, catchUp.threshold, auditIntervalMs, retentionDays

Choose the model and reasoning level from crow models.
Restart Crow after changes with crow service-restart.")]
    Config {
        /// Setting to change; omit to show current settings.
        #[arg(requires = "value")]
        key: Option<String>,
        /// Text, number, true/false, null, or a JSON object as appropriate.
        #[arg(requires = "key")]
        value: Option<String>,
    },
    /// Remove expired review records and local review files.
    Cleanup,
    /// Install the latest Crow release and restart the service.
    Update,
    /// Stop accepting new reviews while current reviews finish.
    Drain,
    /// Accept new reviews again after crow drain.
    Undrain,
    /// Save an encrypted backup of this installation.
    Backup {
        /// Destination archive path.
        file: PathBuf,
        /// File containing the encryption passphrase.
        #[arg(long)]
        passphrase_file: PathBuf,
    },
    /// Restore a backup, replacing settings and history on this machine.
    #[command(
        after_help = "Stop Crow before restoring with crow stop. Existing settings and review history are replaced.
After restoring, run crow login on review workers, then crow start."
    )]
    Restore {
        /// Backup archive to restore.
        file: PathBuf,
        /// File containing the backup's encryption passphrase.
        #[arg(long)]
        passphrase_file: PathBuf,
    },
    #[command(name = "_inspection-mcp", hide = true)]
    InspectionMcp {
        source: PathBuf,
        context: Option<PathBuf>,
    },
}

// Keep the command index task-oriented without changing the command hierarchy.
// Descriptions still come from the subcommands, so detailed help stays in sync.
const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Setup and workers", &["setup", "install", "login", "pair"]),
    ("Repositories", &["enroll", "policy", "repo-config"]),
    (
        "Reviews",
        &[
            "review", "pause", "resume", "restart", "catch-up", "release",
        ],
    ),
    ("Status and troubleshooting", &["status", "doctor", "logs"]),
    (
        "Service",
        &[
            "start",
            "stop",
            "service-restart",
            "run",
            "drain",
            "undrain",
        ],
    ),
    (
        "Settings and maintenance",
        &[
            "config", "models", "update", "cleanup", "backup", "restore", "version", "help",
        ],
    ),
];

fn root_help_template() -> String {
    use std::fmt::Write;

    // Build only the subcommands here. Building Cli would recurse into this template.
    let mut commands = Command::augment_subcommands(clap::Command::new("crow"));
    commands.build();
    let header = commands.get_styles().get_header();
    let literal = commands.get_styles().get_literal();
    let width = commands
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().len())
        .max()
        .unwrap_or_default();
    let mut template =
        format!("{{about}}\n\n{header}Usage:{header:#} {{usage}}\n\n{{before-help}}");
    for (heading, names) in COMMAND_GROUPS {
        writeln!(template, "{header}{heading}:{header:#}").unwrap();
        for name in *names {
            let command = commands.find_subcommand(name).expect("help command exists");
            let about = command.get_about().expect("help command has a description");
            writeln!(template, "  {literal}{name:width$}{literal:#}  {about}").unwrap();
        }
        template.push('\n');
    }
    write!(
        template,
        "{header}Options:{header:#}\n{{options}}{{after-help}}\n"
    )
    .unwrap();
    template
}

#[derive(Args, Debug)]
struct ReviewArgs {
    /// Repository in OWNER/REPO format.
    repo: String,
    /// Pull request number, for example 42.
    #[arg(value_parser = clap::value_parser!(u64).range(1..=9_007_199_254_740_991))]
    number: u64,
}

fn print(format: OutputFormat, command: &str, value: &Value) -> Result<()> {
    match format {
        OutputFormat::Text => println!("{}", crate::output::render(command, value)),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
    }
    Ok(())
}

fn setting_value(key: &str, input: &str) -> Result<Value> {
    if ["worker.model", "worker.effort"].contains(&key) {
        if input == "null" {
            return Ok(Value::Null);
        }
        if input.starts_with('"') {
            return serde_json::from_str(input)
                .context("Use a model or reasoning level as plain text; see crow models");
        }
        return Ok(json!(input));
    }
    serde_json::from_str(input).with_context(|| format!("Invalid value for {key}. Use a number, true/false, or a JSON object. See crow config --help"))
}

pub fn redact(config: &Value) -> Value {
    let mut redacted = config.clone();
    redacted["adminToken"] = json!("[hidden]");
    redacted["worker"]["token"] = json!("[hidden]");
    for path in ["/app", "/pendingApp/app"] {
        if let Some(app) = redacted.pointer_mut(path).filter(|app| app.is_object()) {
            app["pem"] = json!("[hidden]");
            app["webhookSecret"] = json!("[hidden]");
        }
    }
    redacted
}

fn logins(input: &str) -> Result<Vec<String>> {
    let pattern =
        regex::Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?(?:\[bot\])?$")?;
    let mut result = Vec::new();
    for item in input.split(',').map(str::trim) {
        ensure!(
            pattern.is_match(item),
            "Expected comma-separated GitHub logins without empty entries"
        );
        let login = item.to_ascii_lowercase();
        if !result.contains(&login) {
            result.push(login);
        }
    }
    Ok(result)
}

async fn review_command(
    format: OutputFormat,
    config: &Value,
    action: &str,
    review: ReviewArgs,
    model: Option<String>,
    effort: Option<String>,
) -> Result<()> {
    let mut args = json!({"repo": util::repo_name(&review.repo)?, "number": review.number});
    if let Some(model) = model {
        args["model"] = json!(model);
    }
    if let Some(effort) = effort {
        args["effort"] = json!(effort);
    }
    let mut result = operations::admin(config, action, &args).await?;
    if format == OutputFormat::Text && result.is_object() {
        result["repo"] = args["repo"].clone();
        result["number"] = args["number"].clone();
    }
    print(format, action, &result)
}

fn validate_format(format: OutputFormat, command: &Command) -> Result<()> {
    ensure!(
        format != OutputFormat::Json
            || !matches!(
                command,
                Command::Install { .. }
                    | Command::Setup { .. }
                    | Command::Run
                    | Command::Logs
                    | Command::Login
                    | Command::InspectionMcp { .. }
            ),
        "This command uses interactive or streaming output. Run it without --format json"
    );
    Ok(())
}

pub async fn main_cli() -> Result<()> {
    let cli = Cli::parse();
    let format = cli.format;
    if cli.version || matches!(cli.command, Some(Command::Version)) {
        if format == OutputFormat::Json {
            print(
                format,
                "version",
                &json!({"version":env!("CARGO_PKG_VERSION")}),
            )?;
        } else {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        return Ok(());
    }
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    validate_format(format, &command)?;
    let root = config::home();
    match command {
        Command::InspectionMcp { source, context } => {
            return inspection::inspection_main(&source, context.as_deref()).await;
        }
        Command::Setup {
            role,
            ingress,
            port,
        } => {
            let mut opts = json!({});
            if let Some(value) = role {
                opts["role"] = json!(value);
            }
            if let Some(value) = ingress {
                opts["ingress"] = json!(value);
            }
            if let Some(value) = port {
                opts["port"] = json!(value);
            }
            return setup::setup(&root, &opts).await;
        }
        Command::Install { no_setup } => {
            return crate::install::install_command(&root, no_setup).await;
        }
        Command::Backup {
            file,
            passphrase_file,
        } => backup_command(format, &root, &file, &passphrase_file, false),
        Command::Restore {
            file,
            passphrase_file,
        } => backup_command(format, &root, &file, &passphrase_file, true),
        other => return configured_command(format, other, &root).await,
    }
}

fn backup_command(
    format: OutputFormat,
    root: &Path,
    file: &Path,
    passphrase_file: &Path,
    restoring: bool,
) -> Result<()> {
    let mut secret = zeroize::Zeroizing::new(std::fs::read_to_string(passphrase_file)?);
    if secret.ends_with('\n') {
        secret.pop();
        if secret.ends_with('\r') {
            secret.pop();
        }
    }
    let result = if restoring {
        backup::restore_backup(root, file, &secret)?
    } else {
        backup::export_backup(root, file, &secret)?
    };
    print(
        format,
        if restoring { "restore" } else { "backup" },
        &result,
    )
}

async fn configured_command(format: OutputFormat, command: Command, root: &Path) -> Result<()> {
    let mut config = config::load(root)?;
    match command {
        Command::Run => run_crow(config, root.to_path_buf()).await?,
        Command::Start => {
            operations::service_action(root, "start").await?;
            operations::wait_for_startup(root, env!("CARGO_PKG_VERSION")).await?;
            print(format, "start", &json!({"started":true}))?;
        }
        Command::Stop => {
            operations::service_action(root, "stop").await?;
            print(format, "stop", &json!({"stopped":true}))?;
        }
        Command::ServiceRestart => {
            operations::service_action(root, "restart").await?;
            operations::wait_for_startup(root, env!("CARGO_PKG_VERSION")).await?;
            print(format, "service-restart", &json!({"restarted":true}))?;
        }
        Command::Logs => {
            let status = tokio::process::Command::new("journalctl")
                .args(["--user", "-u", &operations::unit_name(root), "-f"])
                .env_clear()
                .envs(util::host_env())
                .status()
                .await
                .context("Could not open the service logs. Crow uses journalctl on Linux")?;
            ensure!(
                status.success(),
                "Could not read the service logs. Run crow doctor to check the installation"
            );
        }
        Command::Doctor { runtime } => {
            let result = operations::doctor(&config, root, runtime).await?;
            print(format, "doctor", &result)?;
            ensure!(result["ok"] == true, "Doctor found problems");
        }
        Command::Login => {
            ensure!(config["role"] != "service", "This host has no worker");
            provider::login(&config["worker"], root).await?;
        }
        Command::Models => print(
            format,
            "models",
            &provider::discover(&config["worker"], root).await?,
        )?,
        Command::Config { key: None, .. } => print(format, "config", &redact(&config))?,
        Command::Config {
            key: Some(key),
            value,
        } => {
            let writable = [
                "worker.concurrency",
                "worker.model",
                "worker.effort",
                "worker.subagents",
                "worker.retry",
                "worker.execution",
                "worker.timeoutMs",
                "catchUp.enabled",
                "catchUp.threshold",
                "auditIntervalMs",
                "retentionDays",
            ];
            ensure!(
                writable.contains(&key.as_str()),
                "Choose a setting from: {}. See crow config --help for examples.",
                writable.join(", ")
            );
            let value = setting_value(&key, value.as_deref().context("Missing setting value")?)?;
            let receipt = json!({"key":key,"value":value});
            let parts: Vec<_> = key.split('.').collect();
            if parts.len() == 2 {
                config[parts[0]][parts[1]] = value;
            } else {
                config[&key] = value;
            }
            config::validate_config(&config)?;
            if ["worker.model", "worker.effort", "worker.subagents"].contains(&key.as_str()) {
                setup::validate_models(&config, root, &config["worker"]).await?;
            }
            config::save(root, &config)?;
            print(format, "config-set", &receipt)?;
        }
        Command::Status => {
            let result = if config["role"] == "worker" {
                let mut status = util::read_json(&root.join("worker-status.json"))?.unwrap_or(
                    json!({"message":"No worker status yet. Run crow start, then crow doctor to check this worker."}),
                );
                if format == OutputFormat::Text && !operations::worker_running(root)? {
                    status["state"] = json!("stopped");
                    status["connection"] = json!("stopped");
                    status["active"] = json!([]);
                    status["message"] = json!(
                        "Worker is not running. Run crow start, then crow doctor to check this worker."
                    );
                }
                status
            } else {
                operations::admin(&config, "status", &Value::Null).await?
            };
            print(format, "status", &result)?;
            let updates = operations::update_availability(root).await?;
            if updates["available"] == true {
                eprintln!("A Crow update is available. Run crow update when ready.");
            }
            if let Some(warning) = updates["warning"].as_str() {
                eprintln!("{warning}");
            }
        }
        Command::Update => print(format, "update", &operations::update(&config, root).await?)?,
        Command::Drain => print(
            format,
            "drain",
            &operations::admin(&config, "drain", &json!({})).await?,
        )?,
        Command::Undrain => print(
            format,
            "undrain",
            &operations::admin(&config, "undrain", &json!({})).await?,
        )?,
        Command::Enroll {
            repo,
            include_backlog,
            reenroll,
            worker,
        } => {
            ensure!(
                config["role"] != "service" || worker.is_some(),
                "Choose a paired worker with --worker ID. Run crow pair first."
            );
            let identity = setup::github_identity(false).await?;
            let args = json!({"repo":util::repo_name(&repo)?,"githubToken":identity["token"],"worker":worker.map(Value::String).unwrap_or_else(||config["worker"]["id"].clone()),"policy":"selected","authors":[config["operator"]],"includeBacklog":include_backlog,"reenroll":reenroll});
            let mut result = operations::admin(&config, "enroll", &args).await?;
            if format == OutputFormat::Text && include_backlog && !reenroll {
                // Enrollment returns the original record, before catch-up clears exclusions.
                result["excluded"] = json!([]);
            }
            print(format, "enroll", &result)?;
        }
        Command::Pair => {
            let worker = json!({"id":util::id(),"token":format!("{}{}",util::id(),util::id())});
            operations::admin(&config, "pair", &worker).await?;
            print(
                format,
                "pair",
                &json!({"serviceUrl":config["publicUrl"],"id":worker["id"],"token":worker["token"]}),
            )?;
        }
        Command::Policy {
            repo,
            authors,
            everyone,
            requesters,
        } => {
            ensure!(
                authors.is_some() || everyone || requesters.is_some(),
                "Specify --authors, --everyone, or --requesters"
            );
            let mut args = json!({"repo":util::repo_name(&repo)?});
            if everyone {
                args["policy"] = json!("everyone");
            }
            if let Some(authors) = authors {
                args["policy"] = json!("selected");
                args["authors"] = json!(logins(&authors)?);
            }
            if let Some(requesters) = requesters {
                args["requesters"] = json!(logins(&requesters)?);
            }
            print(
                format,
                "policy",
                &operations::admin(&config, "config-repo", &args).await?,
            )?;
        }
        Command::RepoConfig {
            repo,
            json: text,
            model,
            effort,
            timeout_seconds,
            reset: _,
        } => {
            let mut values = match text {
                Some(text) => serde_json::from_str(&text)
                    .context("Expected a JSON object for --json. See crow repo-config --help")?,
                None => json!({}),
            };
            if let Some(model) = model {
                values["model"] = json!(model);
            }
            if let Some(effort) = effort {
                values["effort"] = json!(effort);
            }
            if let Some(seconds) = timeout_seconds {
                values["timeoutMs"] = json!(seconds * 1000);
            }
            let overrides = config::parse_repository_settings(&values)?;
            let settings = config::settings(&config, Some(&json!({"settings":overrides})))?;
            setup::validate_models(&config, root, &settings).await?;
            print(
                format,
                "repo-config",
                &operations::admin(
                    &config,
                    "config-repo",
                    &json!({"repo":util::repo_name(&repo)?,"settings":overrides}),
                )
                .await?,
            )?;
        }
        Command::Review(review) => {
            review_command(format, &config, "review", review, None, None).await?
        }
        Command::Pause(review) => {
            review_command(format, &config, "pause", review, None, None).await?
        }
        Command::Restart(review) => {
            review_command(format, &config, "restart", review, None, None).await?
        }
        Command::Resume {
            review,
            model,
            effort,
        } => review_command(format, &config, "resume", review, model, effort).await?,
        Command::CatchUp {
            repo,
            include_backlog,
        } => {
            let mut args = json!({"includeBacklog":include_backlog});
            if let Some(repo) = repo {
                args["repo"] = json!(util::repo_name(&repo)?);
            }
            print(
                format,
                "catch-up",
                &operations::admin(&config, "catch-up", &args).await?,
            )?;
        }
        Command::Release { repo } => {
            let mut args = json!({});
            if let Some(repo) = repo {
                args["repo"] = json!(util::repo_name(&repo)?);
            }
            print(
                format,
                "release",
                &operations::admin(&config, "release", &args).await?,
            )?;
        }
        Command::Cleanup => {
            let mut result = json!({});
            if config["role"] != "worker" {
                result["service"] = operations::admin(&config, "cleanup", &json!({})).await?;
            }
            if config["role"] != "service" {
                let state =
                    crate::worker::worker_request(&config, "maintenance", &json!({})).await?;
                let mut retention_config = config.clone();
                retention_config["retentionDays"] = state["retentionDays"].clone();
                result["worker"] = crate::retention::cleanup(
                    root,
                    &retention_config,
                    state["jobs"]
                        .as_array()
                        .context("Missing maintenance jobs")?,
                )?;
            }
            print(format, "cleanup", &result)?;
        }
        _ => bail!("Command dispatched incorrectly"),
    }
    Ok(())
}

async fn run_crow(config: Value, root: PathBuf) -> Result<()> {
    let _lock = util::acquire_lock(&root.join("runtime.lock"))?;
    let mut service = if config["role"] != "worker" {
        Some(crate::service::start_service(config.clone(), root.clone()).await?)
    } else {
        None
    };
    let worker = if config["role"] != "service" {
        match crate::worker::start_worker(config.clone(), root.clone()).await {
            Ok(worker) => Some(worker),
            Err(error) => {
                if let Some(service) = service.take() {
                    service.close().await?;
                }
                return Err(error);
            }
        }
    } else {
        None
    };
    let result: Result<()> = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut drain = signal(SignalKind::user_defined1())?;
        let mut undrain = signal(SignalKind::user_defined2())?;
        let _ = std::fs::remove_file(root.join("drained.json"));
        let _ = std::fs::remove_file(root.join("undrained.json"));
        util::atomic(&root.join("ready.json"),&json!({"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION")}))?;
        let role = match config["role"].as_str() {
            Some("service") => "service",
            Some("worker") => "worker",
            _ => "service and worker",
        };
        println!("Crow {role} running. Press Ctrl-C to stop.");
        type DrainFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
        let mut draining: Option<DrainFuture<'_>> = None;
        loop {
            tokio::select! {
                biased;
                _ = interrupt.recv() => break,
                _ = terminate.recv() => break,
                _ = drain.recv() => {
                    if draining.is_none() {
                        draining = Some(Box::pin(async {
                            if let Some(worker) = &worker { worker.drain().await?; }
                            util::atomic(&root.join("drained.json"),&json!({"pid":std::process::id(),"at":util::now()}))
                        }));
                    }
                }
                _ = undrain.recv() => {
                    // Drop an unfinished drain wait without cancelling reviews.
                    draining = None;
                    if let Some(worker) = &worker { worker.undrain().await?; }
                    match std::fs::remove_file(root.join("drained.json")) {
                        Ok(()) => {},
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                        Err(error) => return Err(error.into()),
                    }
                    util::atomic(&root.join("undrained.json"), &json!({"pid":std::process::id(),"at":util::now()}))?;
                }
                result = async {
                    match &mut draining { Some(future) => future.await, None => std::future::pending().await }
                } => {
                    result?;
                    draining = None;
                }
            }
        }
        Ok(())
    }.await;
    let _ = std::fs::remove_file(root.join("ready.json"));
    let worker_result = if let Some(worker) = worker {
        worker.close().await
    } else {
        Ok(())
    };
    let service_result = if let Some(service) = service {
        service.close().await
    } else {
        Ok(())
    };
    result.and(worker_result).and(service_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_validation_precedes_side_effects() {
        assert!(Cli::try_parse_from(["crow", "enroll", "owner/repo", "--reenroll"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "crow",
                "policy",
                "owner/repo",
                "--authors",
                "alice",
                "--everyone"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["crow", "review", "owner/repo", "0"]).is_err());
        assert!(
            Cli::try_parse_from(["crow", "review", "owner/repo", "1", "--model", "x"]).is_err()
        );
        assert!(Cli::try_parse_from(["crow", "setup", "--rol", "both"]).is_err());
        assert!(Cli::try_parse_from(["crow", "config", "worker.model"]).is_err());
    }
    #[test]
    fn command_help_is_complete_and_internal_command_is_hidden() {
        use clap::CommandFactory;
        let mut command = Cli::command();
        command.debug_assert();
        command = Cli::command();
        let help = command.render_long_help().to_string();
        assert!(!help.contains("_inspection-mcp"));
        assert!(help.contains("crow status --format json"));
        let grouped_names: Vec<_> = COMMAND_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        let visible_names: std::collections::BTreeSet<_> = command
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .map(|command| command.get_name())
            .collect();
        assert_eq!(
            grouped_names.len(),
            visible_names.len(),
            "duplicate help entries"
        );
        assert_eq!(
            grouped_names
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            visible_names
        );
        for subcommand in command
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
        {
            assert!(
                subcommand.get_about().is_some(),
                "{} needs a description",
                subcommand.get_name()
            );
        }
    }

    #[test]
    fn json_format_is_global_and_preserves_repository_json_input() {
        for args in [
            vec!["crow", "--format", "json", "status"],
            vec!["crow", "status", "--format", "json"],
            vec![
                "crow",
                "repo-config",
                "owner/repo",
                "--json",
                "{}",
                "--format",
                "json",
            ],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().format,
                OutputFormat::Json
            );
        }
        assert!(Cli::try_parse_from(["crow", "status", "--format", "xml"]).is_err());
        let command = Cli::try_parse_from(["crow", "setup", "--format", "json"]).unwrap();
        assert!(validate_format(command.format, command.command.as_ref().unwrap()).is_err());
    }

    #[test]
    fn repository_options_validate_before_side_effects() {
        assert!(Cli::try_parse_from(["crow", "repo-config", "owner/repo"]).is_err());
        assert!(Cli::try_parse_from(["crow", "policy", "owner/repo"]).is_err());
        assert!(
            Cli::try_parse_from([
                "crow",
                "repo-config",
                "owner/repo",
                "--model",
                "gpt-5.4",
                "--effort",
                "high"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["crow", "repo-config", "owner/repo", "--reset"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "crow",
                "repo-config",
                "owner/repo",
                "--reset",
                "--model",
                "gpt-5.4"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "crow",
                "repo-config",
                "owner/repo",
                "--json",
                "{}",
                "--effort",
                "high"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "crow",
                "repo-config",
                "owner/repo",
                "--timeout-seconds",
                "604801"
            ])
            .is_err()
        );
    }

    #[test]
    fn model_and_reasoning_settings_accept_plain_text() {
        assert_eq!(
            setting_value("worker.model", "gpt-5.4").unwrap(),
            json!("gpt-5.4")
        );
        assert_eq!(
            setting_value("worker.effort", "high").unwrap(),
            json!("high")
        );
        assert_eq!(
            setting_value("worker.effort", r#""high""#).unwrap(),
            json!("high")
        );
        assert_eq!(setting_value("worker.model", "null").unwrap(), Value::Null);
        assert_eq!(setting_value("worker.concurrency", "2").unwrap(), json!(2));
        assert_eq!(
            setting_value("catchUp.enabled", "true").unwrap(),
            json!(true)
        );
        assert!(setting_value("worker.concurrency", "two").is_err());
    }

    #[test]
    fn policies_and_redaction() {
        assert_eq!(
            logins("Alice,alice,bob[bot]").unwrap(),
            vec!["alice", "bob[bot]"]
        );
        assert!(logins("alice,").is_err());
        assert!(logins("-alice").is_err());
        let value = json!({"adminToken":"secret","worker":{"token":"secret"},"app":{"pem":"secret","webhookSecret":"secret"}});
        assert!(!redact(&value).to_string().contains("secret"));
        assert_eq!(value["adminToken"], "secret");
    }
}
