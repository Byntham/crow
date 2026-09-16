//! User-facing commands. Clap validates command shape before any side effects.
use crate::{backup, config, inspection, operations, provider, setup, util};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "crow",
    about = "Self-hosted GitHub PR reviews",
    disable_version_flag = true,
    after_help = "CROW_HOME chooses the installation directory. Default: ~/.local/share/crow.\nBrowser URLs printed on a headless host can be opened on another desktop."
)]
struct Cli {
    #[arg(long, action = clap::ArgAction::SetTrue)]
    version: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the executable version.
    Version,
    /// Install this native executable.
    Install {
        #[arg(long)]
        no_setup: bool,
    },
    /// Configure an independent Crow installation.
    Setup {
        #[arg(long, value_parser = ["both", "service", "worker"])]
        role: Option<String>,
        #[arg(long, value_parser = ["funnel", "cloudflare", "existing"])]
        ingress: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Run configured services in the foreground.
    Run,
    Start,
    Stop,
    ServiceRestart,
    Status,
    Doctor {
        #[arg(long)]
        runtime: bool,
    },
    Logs,
    Login,
    Models,
    Enroll {
        repo: String,
        #[arg(long)]
        include_backlog: bool,
        #[arg(long)]
        reenroll: bool,
        #[arg(long)]
        worker: Option<String>,
    },
    Policy {
        repo: String,
        #[arg(long, conflicts_with = "everyone")]
        authors: Option<String>,
        #[arg(long)]
        everyone: bool,
        #[arg(long)]
        requesters: Option<String>,
    },
    RepoConfig {
        repo: String,
        #[arg(long)]
        json: String,
    },
    Review(ReviewArgs),
    Pause(ReviewArgs),
    Resume {
        #[command(flatten)]
        review: ReviewArgs,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
    Restart(ReviewArgs),
    CatchUp {
        repo: Option<String>,
        #[arg(long)]
        include_backlog: bool,
    },
    Release {
        repo: Option<String>,
    },
    Pair,
    Config {
        #[arg(requires = "value")]
        key: Option<String>,
        #[arg(requires = "key")]
        value: Option<String>,
    },
    Cleanup,
    Update,
    Drain,
    Undrain,
    Backup {
        file: PathBuf,
        #[arg(long)]
        passphrase_file: PathBuf,
    },
    Restore {
        file: PathBuf,
        #[arg(long)]
        passphrase_file: PathBuf,
    },
    #[command(name = "_inspection-mcp", hide = true)]
    InspectionMcp {
        source: PathBuf,
        context: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
struct ReviewArgs {
    repo: String,
    #[arg(value_parser = clap::value_parser!(u64).range(1..=9_007_199_254_740_991))]
    number: u64,
}

fn print(value: &Value) -> Result<()> {
    if let Some(text) = value.as_str() {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

pub fn redact(config: &Value) -> Value {
    let mut redacted = config.clone();
    redacted["adminToken"] = json!("[hidden]");
    redacted["worker"]["token"] = json!("[hidden]");
    if redacted["app"].is_object() {
        redacted["app"]["pem"] = json!("[hidden]");
        redacted["app"]["webhookSecret"] = json!("[hidden]");
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
    print(&operations::admin(config, action, &args).await?)
}

pub async fn main_cli() -> Result<()> {
    let cli = Cli::parse();
    if cli.version || matches!(cli.command, Some(Command::Version)) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
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
        } => backup_command(&root, &file, &passphrase_file, false),
        Command::Restore {
            file,
            passphrase_file,
        } => backup_command(&root, &file, &passphrase_file, true),
        other => return configured_command(other, &root).await,
    }
}

fn backup_command(root: &Path, file: &Path, passphrase_file: &Path, restoring: bool) -> Result<()> {
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
    print(&result)
}

async fn configured_command(command: Command, root: &Path) -> Result<()> {
    let mut config = config::load(root)?;
    match command {
        Command::Run => run_crow(config, root.to_path_buf()).await?,
        Command::Start => {
            operations::service_action(root, "start").await?;
            operations::wait_for_startup(root, env!("CARGO_PKG_VERSION")).await?;
        }
        Command::Stop => operations::service_action(root, "stop").await?,
        Command::ServiceRestart => {
            operations::service_action(root, "restart").await?;
            operations::wait_for_startup(root, env!("CARGO_PKG_VERSION")).await?;
        }
        Command::Logs => {
            let status = tokio::process::Command::new("journalctl")
                .args(["--user", "-u", &operations::unit_name(root), "-f"])
                .env_clear()
                .envs(util::host_env())
                .status()
                .await?;
            ensure!(status.success(), "journalctl failed");
        }
        Command::Doctor { runtime } => {
            let result = operations::doctor(&config, root, runtime).await?;
            print(&result)?;
            ensure!(result["ok"] == true, "Doctor found problems");
        }
        Command::Login => {
            ensure!(config["role"] != "service", "This host has no worker");
            provider::login(&config["worker"], root).await?;
        }
        Command::Models => print(&provider::discover(&config["worker"], root).await?)?,
        Command::Config { key: None, .. } => print(&redact(&config))?,
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
                "worker.timeoutMs",
                "catchUp.enabled",
                "catchUp.threshold",
                "auditIntervalMs",
                "retentionDays",
            ];
            ensure!(
                writable.contains(&key.as_str()),
                "Set one of: {}. Values use JSON syntax.",
                writable.join(", ")
            );
            let value: Value =
                serde_json::from_str(value.as_deref().context("Missing JSON value")?)?;
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
            println!("Configuration saved. Run crow service-restart to apply it.");
        }
        Command::Status => {
            if config["role"] == "worker" {
                print(
                    &util::read_json(&root.join("worker-status.json"))?.unwrap_or(
                        json!({"message":"Run crow doctor and crow logs for worker status."}),
                    ),
                )?;
            } else {
                print(&operations::admin(&config, "status", &Value::Null).await?)?;
            }
            let updates = operations::update_availability(root).await?;
            if updates["available"] == true {
                println!("A Crow update is available. Run crow update when ready.");
            }
            if let Some(warning) = updates["warning"].as_str() {
                eprintln!("{warning}");
            }
        }
        Command::Update => print(&operations::update(&config, root).await?)?,
        Command::Drain => print(&operations::admin(&config, "drain", &json!({})).await?)?,
        Command::Undrain => print(&operations::admin(&config, "undrain", &json!({})).await?)?,
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
            print(&operations::admin(&config, "enroll", &args).await?)?;
        }
        Command::Pair => {
            let worker = json!({"id":util::id(),"token":format!("{}{}",util::id(),util::id())});
            print(&operations::admin(&config, "pair", &worker).await?)?;
            print(
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
            print(&operations::admin(&config, "config-repo", &args).await?)?;
        }
        Command::RepoConfig { repo, json: text } => {
            let overrides = config::parse_repository_settings(&serde_json::from_str(&text)?)?;
            let settings = config::settings(&config, Some(&json!({"settings":overrides})))?;
            setup::validate_models(&config, root, &settings).await?;
            print(
                &operations::admin(
                    &config,
                    "config-repo",
                    &json!({"repo":util::repo_name(&repo)?,"settings":overrides}),
                )
                .await?,
            )?;
        }
        Command::Review(review) => review_command(&config, "review", review, None, None).await?,
        Command::Pause(review) => review_command(&config, "pause", review, None, None).await?,
        Command::Restart(review) => review_command(&config, "restart", review, None, None).await?,
        Command::Resume {
            review,
            model,
            effort,
        } => review_command(&config, "resume", review, model, effort).await?,
        Command::CatchUp {
            repo,
            include_backlog,
        } => {
            let mut args = json!({"includeBacklog":include_backlog});
            if let Some(repo) = repo {
                args["repo"] = json!(util::repo_name(&repo)?);
            }
            print(&operations::admin(&config, "catch-up", &args).await?)?;
        }
        Command::Release { repo } => {
            let mut args = json!({});
            if let Some(repo) = repo {
                args["repo"] = json!(util::repo_name(&repo)?);
            }
            print(&operations::admin(&config, "release", &args).await?)?;
        }
        Command::Cleanup => {
            if config["role"] != "worker" {
                print(&operations::admin(&config, "cleanup", &json!({})).await?)?;
            }
            if config["role"] != "service" {
                let state =
                    crate::worker::worker_request(&config, "maintenance", &json!({})).await?;
                let mut retention_config = config.clone();
                retention_config["retentionDays"] = state["retentionDays"].clone();
                print(&crate::retention::cleanup(
                    root,
                    &retention_config,
                    state["jobs"]
                        .as_array()
                        .context("Missing maintenance jobs")?,
                )?)?;
            }
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
        println!("Crow {} running.", config["role"].as_str().unwrap_or("both"));
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
