//! Resumable installation and onboarding. Each external side effect is preceded
//! by persisting enough information to resume without replacing another route.
use crate::github::GitHubApi;
use crate::{config, operations, util};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, oneshot};

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

async fn prompt_line(input: &mut crate::process::NonblockingIo<'_>) -> io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    loop {
        // Read only through the newline. Later prompts and child processes
        // must retain any answers already waiting on the same stdin.
        let mut byte = [0_u8];
        if input.read(&mut byte).await? == 0 {
            break;
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len().is_multiple_of(1024) {
            tokio::task::yield_now().await;
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn ask(label: &str, fallback: &str) -> Result<String> {
    let mut input = crate::process::NonblockingIo::stdin()?;
    print!(
        "{label}{}: ",
        if fallback.is_empty() {
            String::new()
        } else {
            format!(" [{fallback}]")
        }
    );
    io::stdout().flush()?;
    let answer = tokio::select! {
        biased;
        result = operations::interrupted() => { result?; bail!("Setup interrupted. Run crow setup to continue."); }
        answer = prompt_line(&mut input) => answer?,
    };
    if answer.is_empty() {
        bail!("Setup requires interactive input. Run crow setup in a terminal.");
    }
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        fallback.to_owned()
    } else {
        answer.to_owned()
    })
}

async fn choose(label: &str, fallback: &str, choices: &[(&str, &str)]) -> Result<String> {
    println!("\n{label}");
    for (index, (value, description)) in choices.iter().enumerate() {
        if description.is_empty() || description == value {
            println!("  {}. {value}", index + 1);
        } else {
            println!("  {}. {value:<12} {description}", index + 1);
        }
    }
    loop {
        let answer = ask("Choose a number or name", fallback).await?;
        if let Ok(number) = answer.parse::<usize>()
            && let Some((value, _)) = number.checked_sub(1).and_then(|i| choices.get(i))
        {
            return Ok((*value).to_owned());
        }
        if let Some((value, _)) = choices
            .iter()
            .find(|(value, _)| value.eq_ignore_ascii_case(&answer))
        {
            return Ok((*value).to_owned());
        }
        println!("Choose one of the options above. Press Enter to keep {fallback}.");
    }
}

async fn confirm(label: &str, fallback: bool) -> Result<bool> {
    loop {
        let answer = ask(label, if fallback { "Y/n" } else { "y/N" }).await?;
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            "y/n" => return Ok(fallback),
            _ => println!("Enter yes or no."),
        }
    }
}

pub async fn ensure_command(command: &str) -> Result<()> {
    if operations::run(command, &["--version"], false)
        .await
        .is_ok()
    {
        return Ok(());
    }
    if matches!(command, "gh" | "git") && !HostRuntimeSetup.supports_apt() {
        bail!(
            "{command} is missing. Guided package installation supports Ubuntu/Debian with apt-get. Install {command} using this distribution's package manager, then rerun crow setup."
        );
    }
    if !cfg!(target_os = "linux")
        || !confirm(
            &format!("{command} is missing. Install the official Linux package using sudo?"),
            true,
        )
        .await?
    {
        bail!("Install {command}, then rerun crow setup.");
    }
    if matches!(command, "gh" | "git") {
        operations::run("sudo", &["apt-get", "update"], true).await?;
        operations::run("sudo", &["apt-get", "install", "-y", command], true)
            .await
            .with_context(|| format!("Could not install {command}. Resolve the package-manager error above or install {command} from its official installation instructions, then rerun crow setup."))?;
    } else {
        let directory = tempfile::tempdir()?;
        let (url, filename, installer) = match command {
            "tailscale" => (
                "https://tailscale.com/install.sh".to_owned(),
                "install.sh",
                "sh",
            ),
            "cloudflared" => {
                let cpu = match std::env::consts::ARCH {
                    "x86_64" => "amd64",
                    "aarch64" => "arm64",
                    _ => bail!("Install cloudflared manually for this CPU architecture."),
                };
                (
                    format!(
                        "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-{cpu}.deb"
                    ),
                    "cloudflared.deb",
                    "dpkg",
                )
            }
            _ => bail!("Automatic installation is unavailable for {command}."),
        };
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?
            .get(url)
            .send()
            .await?
            .error_for_status()?;
        let file = directory.path().join(filename);
        util::atomic_bytes(&file, &response.bytes().await?)?;
        let file = file.to_string_lossy();
        let args = if installer == "dpkg" {
            vec![installer, "-i", &file]
        } else {
            vec![installer, &file]
        };
        operations::run("sudo", &args, true).await?;
    }
    operations::run(command, &["--version"], false).await?;
    Ok(())
}

const RUNTIME_PREREQUISITES: &[(&str, &str)] = &[
    ("podman", "podman"),
    ("newuidmap", "uidmap"),
    ("newgidmap", "uidmap"),
    ("slirp4netns", "slirp4netns"),
    ("fuse-overlayfs", "fuse-overlayfs"),
];

fn runtime_requested(settings: &Value, role: &str) -> bool {
    role != "service"
        && (settings["execution"]["automatic"] == true
            || settings["execution"]["repositories"]
                .as_object()
                .is_some_and(|repos| !repos.is_empty()))
}

fn runtime_install_hint() -> &'static str {
    "Check the reported error with: podman info --format=json\n\n\
On Ubuntu/Debian, the required packages can be installed with:\n  \
sudo apt-get update && sudo apt-get install -y podman uidmap slirp4netns fuse-overlayfs dbus-user-session\n\n\
Host configuration, if the error requires it:\n\
- Missing UID/GID mappings: have an administrator assign non-overlapping ranges in /etc/subuid and /etc/subgid, then run podman system migrate.\n\
- Cgroup v1: use a cgroup-v2 host or ask its administrator to enable the unified hierarchy and reboot.\n\
- Blocked user namespaces or seccomp: ask the host administrator to permit rootless Podman.\n\n\
Run Crow as your normal user. Rerun crow setup after fixing the reported issue."
}

#[async_trait::async_trait]
trait RuntimeSetupBackend: Send + Sync {
    async fn check(&self, settings: &Value) -> Result<()>;
    fn available(&self, command: &str) -> bool;
    fn supports_apt(&self) -> bool;
    async fn confirm(&self, label: &str, fallback: bool) -> Result<bool>;
    async fn install(&self, packages: &[String]) -> Result<()>;
}

struct HostRuntimeSetup;
#[async_trait::async_trait]
impl RuntimeSetupBackend for HostRuntimeSetup {
    async fn check(&self, settings: &Value) -> Result<()> {
        crate::execution::check_prerequisites(settings).await
    }
    fn available(&self, command: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let executable = |path: &Path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        };
        if command.contains('/') {
            return executable(Path::new(command));
        }
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|directory| executable(&directory.join(command)))
    }
    fn supports_apt(&self) -> bool {
        cfg!(target_os = "linux") && self.available("apt-get")
    }
    async fn confirm(&self, label: &str, fallback: bool) -> Result<bool> {
        confirm(label, fallback).await
    }
    async fn install(&self, packages: &[String]) -> Result<()> {
        operations::run("sudo", &["apt-get", "update"], true).await?;
        let mut args = vec!["apt-get", "install", "-y"];
        args.extend(packages.iter().map(String::as_str));
        operations::run("sudo", &args, true).await?;
        Ok(())
    }
}

async fn runtime_setup_with_backend(
    settings: &mut Value,
    role: &str,
    backend: &impl RuntimeSetupBackend,
) -> Result<()> {
    if !runtime_requested(settings, role) {
        return Ok(());
    }
    let initial_check = backend.check(settings).await;
    let podman = settings["execution"]["podman"].as_str().unwrap_or("podman");
    // A custom executable can be a wrapper or remote-environment launcher.
    // Installing another Podman cannot repair that operator-selected path.
    let managed_command = podman == "podman";
    let mut packages = Vec::new();
    if managed_command {
        for (command, package) in RUNTIME_PREREQUISITES {
            // newuidmap/newgidmap do not provide a reliable --version exit code.
            let required = matches!(*command, "podman" | "newuidmap" | "newgidmap");
            if (required || initial_check.is_err())
                && !backend.available(command)
                && !packages.iter().any(|known| known == package)
            {
                packages.push((*package).to_owned());
            }
        }
        if !packages.is_empty() {
            // A user session bus is required by rootless Podman's systemd cgroup
            // manager, but no binary reliably identifies dbus-user-session.
            packages.push("dbus-user-session".to_owned());
        }
    }
    let mut failure = match initial_check {
        Ok(()) if packages.is_empty() => return Ok(()),
        Ok(()) => anyhow::anyhow!(
            "Podman is available, but required rootless helper commands are missing"
        ),
        Err(error) => error,
    };
    if !packages.is_empty()
        && backend.supports_apt()
        && backend
            .confirm(
                &format!(
                    "Runtime testing needs Linux packages ({}). Install them using sudo?",
                    packages.join(", ")
                ),
                true,
            )
            .await?
    {
        match backend.install(&packages).await {
            Ok(()) => match backend.check(settings).await {
                Ok(()) if backend.available("newuidmap") && backend.available("newgidmap") => {
                    return Ok(());
                }
                Ok(()) => {
                    failure = anyhow::anyhow!(
                        "Package installation completed, but newuidmap or newgidmap is still unavailable"
                    )
                }
                Err(error) => failure = error,
            },
            Err(error) => failure = error.context("Runtime package installation failed"),
        }
    }
    println!(
        "\nRuntime testing is not ready: {failure:#}\n{}",
        runtime_install_hint()
    );
    if !managed_command {
        println!(
            "The configured Podman executable is {podman:?}. Correct worker.execution.podman or make that executable available; Crow will not replace a custom executable."
        );
    }
    if backend
        .confirm(
            "Continue setup with runtime testing disabled for this worker?",
            false,
        )
        .await?
    {
        settings["execution"]["automatic"] = json!(false);
        settings["execution"]["repositories"] = json!({});
        println!(
            "Runtime testing is disabled. Crow will review source without running it. Re-enable worker.execution when this host is ready."
        );
        return Ok(());
    }
    Err(failure.context("Runtime setup is incomplete. Fix the reported host prerequisite and rerun crow setup, or rerun setup and choose to continue with runtime testing disabled"))
}

async fn ensure_runtime_prerequisites(config: &mut Value, role: &str, root: &Path) -> Result<()> {
    // Keep cancellation outside error recovery so Ctrl-C during installation
    // cannot be mistaken for a request to disable runtime testing.
    tokio::select! {
        biased;
        interrupt = operations::interrupted() => {
            interrupt?;
            bail!("Setup interrupted. Run crow setup to continue.");
        }
        result = runtime_setup_with_backend(&mut config["worker"], role, &HostRuntimeSetup) => result?,
    }
    config::save(root, config)?;
    Ok(())
}

pub fn choose_funnel_port(
    status: &Value,
    previous: Option<u16>,
    target: Option<&str>,
) -> Result<u16> {
    let object = status
        .as_object()
        .context("Invalid Tailscale Serve status")?;
    let mut used = HashSet::new();
    if let Some(tcp) = object.get("TCP").filter(|v| !v.is_null()) {
        for key in tcp
            .as_object()
            .context("Invalid Tailscale TCP routes")?
            .keys()
        {
            if let Ok(port) = key.parse::<u16>() {
                used.insert(port);
            }
        }
    }
    let mut routes = Vec::new();
    if let Some(web) = object.get("Web").filter(|v| !v.is_null()) {
        for (host, route) in web.as_object().context("Invalid Tailscale web routes")? {
            route.as_object().context("Invalid Tailscale web route")?;
            if let Some(handlers) = route.get("Handlers").filter(|v| !v.is_null()) {
                for handler in handlers
                    .as_object()
                    .context("Invalid Tailscale handlers")?
                    .values()
                {
                    handler.as_object().context("Invalid Tailscale handler")?;
                }
            }
            if let Some(port) = host.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
                used.insert(port);
                routes.push((port, route));
            }
        }
    }
    if let Some(previous) = previous.filter(|p| [443, 8443, 10000].contains(p)) {
        if !used.contains(&previous) {
            return Ok(previous);
        }
        let matching: Vec<_> = routes
            .iter()
            .filter(|(port, _)| *port == previous)
            .collect();
        if matching.len() == 1
            && let Some(handlers) = matching[0].1["Handlers"].as_object()
            && handlers.len() == 1
            && target.is_some()
            && handlers.get("/").and_then(|v| v["Proxy"].as_str()) == target
        {
            return Ok(previous);
        }
    }
    [8443, 10000, 443].into_iter().find(|p| !used.contains(p)).context("All supported Funnel ports are in use. Free one or choose another HTTPS provider; Crow will not overwrite existing routes.")
}

async fn tailscale_mutate(args: &[&str]) -> Result<()> {
    match operations::run("tailscale", args, true).await {
        Ok(_) => Ok(()),
        Err(error) => {
            println!("Tailscale could not complete the command. Check its message above.");
            if !confirm("If it needs system permissions, retry with sudo?", false).await? {
                return Err(error);
            }
            let mut sudo = vec!["tailscale"];
            sudo.extend_from_slice(args);
            operations::run("sudo", &sudo, true).await?;
            Ok(())
        }
    }
}

pub async fn configure_funnel(config: &mut Value, root: &Path) -> Result<()> {
    let mut state: Value = serde_json::from_str(
        &operations::run("tailscale", &["status", "--json"], false)
            .await
            .context("Tailscale is needed for Funnel. Install it and rerun crow setup.")?,
    )?;
    if state["BackendState"] != "Running" {
        tailscale_mutate(&["up"]).await?;
        state = serde_json::from_str(
            &operations::run("tailscale", &["status", "--json"], false).await?,
        )?;
    }
    let dns = state["Self"]["DNSName"]
        .as_str()
        .map(|v| v.trim_end_matches('.'))
        .filter(|v| !v.is_empty())
        .context(
            "Tailscale did not report this host's DNS name. Enable MagicDNS/HTTPS and retry.",
        )?;
    let status: Value = serde_json::from_str(
        &operations::run("tailscale", &["serve", "status", "--json"], false).await?,
    )?;
    let target = format!("http://127.0.0.1:{}", config["port"]);
    let previous = config["ingress"]["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok());
    let port = choose_funnel_port(&status, previous, Some(&target))?;
    let public_url = util::https_url(&format!(
        "https://{dns}{}",
        if port == 443 {
            String::new()
        } else {
            format!(":{port}")
        }
    ))?;
    if config["ingress"]["pending"] == true
        && (previous != Some(port) || config["publicUrl"] != public_url)
    {
        bail!(
            "The pending Crow Funnel route has changed. Restore its recorded hostname and dedicated port before resuming setup."
        );
    }
    config["ingress"] = json!({"type":"funnel","port":port,"target":target,"pending":true});
    config["publicUrl"] = json!(public_url);
    config::save(root, config)?;
    tailscale_mutate(&["funnel", "--bg", &format!("--https={port}"), &target]).await?;
    config["ingress"].as_object_mut().unwrap().remove("pending");
    config::save(root, config)?;
    Ok(())
}

async fn configure_cloudflare(config: &mut Value, root: &Path, hostname: &str) -> Result<()> {
    let origin = util::https_url(&format!("https://{hostname}"))?;
    let parsed = url::Url::parse(&origin)?;
    let hostname = parsed.host_str().context("Missing Cloudflare hostname")?;
    let cf = user_home()?.join(".cloudflared");
    let saved = root.join("cloudflare-created.json");
    if util::read_json(&saved)?.is_none() {
        if !cf.join("cert.pem").try_exists()? {
            operations::run("cloudflared", &["tunnel", "login"], true).await?;
        }
        let worker = string(&config["worker"], "id");
        let name = format!("crow-{}", worker.chars().take(12).collect::<String>());
        let mut tunnels = cloudflare_tunnels().await?;
        if !tunnels.iter().any(|t| t["name"] == name) {
            operations::run("cloudflared", &["tunnel", "create", &name], true).await?;
            tunnels = cloudflare_tunnels().await?;
        }
        let id = tunnels
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t["id"].as_str())
            .context("Cloudflare did not return the named tunnel ID.")?;
        util::atomic(&saved, &json!({"id":id,"name":name}))?;
    }
    let saved = util::read_json(&saved)?.context("Missing saved Cloudflare tunnel")?;
    let id = saved["id"].as_str().context("Invalid saved tunnel ID")?;
    // This identifier is used as a path component, including when loaded from disk.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("Invalid saved tunnel ID");
    }
    let credentials = std::fs::read(cf.join(format!("{id}.json")))?;
    let credentials_file = root.join("cloudflare-credentials.json");
    util::atomic_bytes(&credentials_file, &credentials)?;
    operations::run(
        "cloudflared",
        &["tunnel", "route", "dns", id, hostname],
        true,
    )
    .await?;
    let file = root.join("cloudflare.json");
    util::atomic(
        &file,
        &json!({"tunnel":id,"credentials-file":credentials_file,"ingress":[{"hostname":hostname,"service":format!("http://127.0.0.1:{}",config["port"])},{"service":"http_status:404"}]}),
    )?;
    config["ingress"] = json!({"type":"cloudflare","file":file,"tunnel":id});
    config["publicUrl"] = json!(format!("https://{hostname}"));
    Ok(())
}

async fn cloudflare_tunnels() -> Result<Vec<Value>> {
    let tunnels: Value = serde_json::from_str(
        &operations::run(
            "cloudflared",
            &["tunnel", "list", "--output", "json"],
            false,
        )
        .await?,
    )?;
    let tunnels = tunnels
        .as_array()
        .context("Invalid Cloudflare tunnel list")?;
    for tunnel in tunnels {
        if !tunnel["id"].is_string() || !tunnel["name"].is_string() {
            bail!("Invalid Cloudflare tunnel");
        }
    }
    Ok(tunnels.clone())
}

pub fn app_manifest(config: &Value, name: &str) -> Value {
    let origin = string(config, "publicUrl");
    json!({"name":name,"url":origin,"hook_attributes":{"url":format!("{origin}/webhooks/github"),"active":true},"redirect_url":format!("{origin}/setup/callback"),"public":config["appRegistration"]["visibility"] != "private","default_permissions":{"contents":"read","metadata":"read","pull_requests":"write","issues":"write"},"default_events":["pull_request","push","issue_comment"]})
}

pub fn validate_app_requirements(app: &Value) -> Result<()> {
    if !matches!(
        app["permissions"]["contents"].as_str(),
        Some("read" | "write")
    ) || app["permissions"]["pull_requests"] != "write"
        || app["permissions"]["issues"] != "write"
    {
        bail!(
            "The GitHub App needs Contents: read, Pull requests: write, and Issues: write. Update its permissions in GitHub App settings and approve any pending installation changes, then rerun crow setup."
        );
    }
    for event in ["pull_request", "push", "issue_comment"] {
        if !app["events"]
            .as_array()
            .is_some_and(|events| events.contains(&json!(event)))
        {
            bail!(
                "The GitHub App must subscribe to {event}. Update its events in GitHub App settings, then rerun crow setup."
            );
        }
    }
    Ok(())
}

pub fn validate_app_webhook(hook: &Value, expected: &str) -> Result<()> {
    if hook["url"] != expected || hook["content_type"] != "json" || hook["insecure_ssl"] != "0" {
        bail!(
            "The GitHub App webhook must use {expected}, JSON content, and SSL verification. Check the App's webhook settings."
        );
    }
    Ok(())
}

// Persist the secret before changing GitHub. A timeout may mean the PATCH succeeded;
// retries must reuse that secret rather than orphaning the App connection.
async fn finish_existing_app(
    config: &mut Value,
    root: &Path,
    github: &dyn GitHubApi,
    token: &str,
) -> Result<()> {
    if !config["app"].is_null() {
        bail!(
            "An App is already configured. Connecting another App requires an explicit migration."
        );
    }
    config::save(root, config)?;
    let pending = &config["pendingApp"];
    let expected = format!("{}/webhooks/github", string(config, "publicUrl"));
    if pending["webhookUrl"] != expected {
        bail!(
            "The public HTTPS address changed during App connection. Restore the saved address before rerunning setup."
        );
    }
    github.request("/app/hook/config", Some(token), "PATCH", Some(&json!({
        "url":expected, "content_type":"json", "insecure_ssl":"0",
        "secret":pending["app"]["webhookSecret"]
    }))).await.context("Could not configure the App webhook. Credentials are saved; rerun crow setup to retry.")?;
    let hook = github
        .request("/app/hook/config", Some(token), "GET", None)
        .await?;
    validate_app_webhook(&hook, &expected)?;
    let mut completed = config.clone();
    completed["app"] = pending["app"].clone();
    completed.as_object_mut().unwrap().remove("pendingApp");
    config::save(root, &completed)?;
    *config = completed;
    Ok(())
}

async fn connect_existing_app(config: &mut Value, root: &Path) -> Result<()> {
    let mut app = if config["pendingApp"].is_object() {
        println!("Resuming the saved GitHub App connection.");
        config["pendingApp"]["app"].clone()
    } else {
        println!(
            "\nOpen your GitHub App's settings. You need permission to manage the App, not just an installation. Generate or locate its private key and copy the PEM file to this machine."
        );
        let id = ask("GitHub App ID", "")
            .await?
            .parse::<u64>()
            .context("Enter the numeric App ID, not the client ID or installation ID")?;
        if id == 0 {
            bail!("The GitHub App ID must be positive");
        }
        let path = ask("Private-key PEM file path", "").await?;
        let path = if let Some(relative) = path.strip_prefix("~/") {
            user_home()?.join(relative)
        } else {
            PathBuf::from(path)
        };
        let pem =
            std::fs::read_to_string(path).context("Could not read the private-key PEM file")?;
        json!({"id":id,"pem":pem,"webhookSecret":format!("{}{}",util::id(),util::id()),"botId":null})
    };
    let token = crate::github::jwt(&app)?;
    let github = crate::github::GitHub::new(Some(app.clone()));
    let metadata = github.request("/app", Some(&token), "GET", None).await?;
    if metadata["id"] != app["id"] {
        bail!("GitHub authenticated a different App ID");
    }
    validate_app_requirements(&metadata)?;
    let slug = string(&metadata, "slug");
    if slug.is_empty() || !slug.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        bail!("GitHub returned an invalid App slug");
    }
    app["slug"] = json!(slug);
    let hook = github
        .request("/app/hook/config", Some(&token), "GET", None)
        .await?;
    let expected = format!("{}/webhooks/github", string(config, "publicUrl"));
    if config["pendingApp"].is_object() && config["pendingApp"]["webhookUrl"] != expected {
        bail!(
            "The public HTTPS address changed during App connection. Restore the saved address before rerunning setup."
        );
    }
    println!(
        "\nConnect GitHub App {slug} (ID {})\nCurrent webhook: {}\nCrow webhook: {expected}\n\nCrow will replace this App's webhook URL and secret, use JSON, and enable SSL verification. Keep Webhook Active enabled in the App's GitHub settings.\nUse one Crow service per App. Stop any previous service before continuing, even if its webhook URL is the same. Workers should pair with this service.\nConnecting the App does not restore review history, repository assignments, or worker sessions. Use crow backup/restore to preserve service state when moving an installation.",
        app["id"],
        string(&hook, "url")
    );
    if !confirm(
        "Is the previous service stopped, if any, and should Crow configure this App's webhook?",
        false,
    )
    .await?
    {
        bail!("App connection cancelled. GitHub was not changed.");
    }
    config["pendingApp"] = json!({"app":app,"webhookUrl":expected});
    // The operator may spend longer than a JWT's lifetime checking the old service.
    let token = crate::github::jwt(&config["pendingApp"]["app"])?;
    finish_existing_app(config, root, &github, &token).await
}

pub fn registration_action(registration: &Value, state: &str) -> Result<String> {
    if state.len() != 64
        || !state
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("Invalid registration state");
    }
    if registration["ownerType"] == "personal" {
        return Ok(format!(
            "https://github.com/settings/apps/new?state={state}"
        ));
    }
    let organization = string(registration, "organization");
    if registration["ownerType"] != "organization"
        || organization.is_empty()
        || organization.len() > 39
        || !organization
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || organization.starts_with('-')
        || organization.ends_with('-')
    {
        bail!("Invalid GitHub organization name");
    }
    Ok(format!(
        "https://github.com/organizations/{organization}/settings/apps/new?state={state}"
    ))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn setup_page(title: &str, step: &str, body: &str) -> String {
    include_str!("setup_page.html")
        .replace("__TITLE__", &html_escape(title))
        .replace("__STEP__", &html_escape(step))
        .replace("__BODY__", body)
}

struct RegistrationState {
    config: Mutex<Value>,
    root: PathBuf,
    state: String,
    secret_path: String,
    action: String,
    manifest: Value,
    busy: AtomicBool,
    completed: Mutex<Option<oneshot::Sender<Result<()>>>>,
    client: reqwest::Client,
    conversion_base: url::Url,
}

fn setup_response(status: StatusCode, body: String, content_type: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn registration_handler(
    State(state): State<Arc<RegistrationState>>,
    request: Request,
) -> Response {
    if request.method() != axum::http::Method::GET {
        return setup_response(StatusCode::NOT_FOUND, "Not found".into(), "text/plain");
    }
    let uri = request.uri();
    if uri.path() == "/health" {
        return setup_response(
            StatusCode::OK,
            json!({"ok":true,"setup":true}).to_string(),
            "application/json",
        );
    }
    if util::equal(uri.path(), &state.secret_path) {
        return setup_response(
            StatusCode::OK,
            setup_page(
                "Connect Crow to GitHub",
                "Step 1 of 2 · Create your app",
                &format!(
                    "<p>Create a GitHub App so Crow can read pull requests and post reviews on your behalf.</p><p>GitHub will show the requested permissions and ask you to confirm. You will then choose which repositories Crow can access.</p><form action=\"{}\" method=\"post\"><input type=\"hidden\" name=\"manifest\" value=\"{}\"><button>Create GitHub App</button></form><p class=\"note\">Keep your Crow setup terminal open while you complete these steps.</p>",
                    html_escape(&state.action),
                    html_escape(&state.manifest.to_string())
                ),
            ),
            "text/html",
        );
    }
    if uri.path() != "/setup/callback" {
        return setup_response(StatusCode::NOT_FOUND, "Not found".into(), "text/plain");
    }
    let query: Vec<_> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect();
    let supplied_state = query
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let code = query
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if !util::equal(supplied_state, &state.state)
        || code.is_empty()
        || state
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return setup_response(
            StatusCode::BAD_REQUEST,
            setup_page(
                "This setup link is no longer valid",
                "Return to your terminal",
                "<p>The link may have already been used, or it may belong to another setup session.</p><p>Check your Crow setup terminal for the next step. If setup has stopped, run <code>crow setup</code> to continue.</p>",
            ),
            "text/html",
        );
    }
    let result: Result<String> = async {
        let mut endpoint = state.conversion_base.clone();
        endpoint.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid GitHub API URL"))?.pop_if_empty().push(code).push("conversions");
        let app: Value = state.client.post(endpoint).header("Accept", "application/vnd.github+json").send().await?.error_for_status()?.json().await?;
        if !(app["id"].as_u64().is_some_and(|id| id > 0) || app["id"].as_str().is_some_and(|id| !id.is_empty())) || ["pem", "webhook_secret", "slug"].iter().any(|key| string(&app, key).is_empty()) { bail!("GitHub returned incomplete App credentials."); }
        let mut config = state.config.lock().await;
        config["app"] = json!({"id":app["id"],"pem":app["pem"],"webhookSecret":app["webhook_secret"],"slug":app["slug"],"botId":null});
        config::save(&state.root, &config)?;
        let mut install = url::Url::parse("https://github.com/apps/")?;
        install.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid GitHub URL"))?.pop_if_empty().push(string(&app, "slug")).push("installations").push("new");
        Ok(setup_page("Your GitHub App is ready", "Step 2 of 2 · Choose repositories", &format!("<p>Install your app on GitHub and choose the repositories it can access.</p><a class=\"button\" href=\"{}\">Choose repositories on GitHub</a><p>Then return to your terminal and press Enter to continue setup.</p><p class=\"note\">You will choose which repositories Crow should review in the terminal.</p>",html_escape(install.as_str()))))
    }.await;
    let (response, outcome) = match result {
        Ok(body) => (setup_response(StatusCode::OK, body, "text/html"), Ok(())),
        Err(error) => (
            setup_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                setup_page(
                    "Could not finish connecting to GitHub",
                    "Return to your terminal",
                    "<p>Crow could not complete the GitHub App registration.</p><p>Your Crow setup terminal has the error details. Resolve the issue there, then run <code>crow setup</code> to continue.</p>",
                ),
                "text/html",
            ),
            Err(error),
        ),
    };
    if let Some(completed) = state.completed.lock().await.take() {
        let _ = completed.send(outcome);
    }
    response
}

pub async fn register_app(config: &mut Value, root: &Path) -> Result<()> {
    let state = format!("{}{}", util::id(), util::id());
    let secret = format!("{}{}", util::id(), util::id());
    let previous = config
        .get("appRegistration")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"ownerType":"personal","visibility":"public"}));
    let owner_type = choose(
        "Who should own the GitHub App?",
        string(&previous, "ownerType"),
        &[
            ("personal", "Your GitHub account"),
            ("organization", "A GitHub organization you administer"),
        ],
    )
    .await?;
    let organization = if owner_type == "organization" {
        Some(
            ask(
                "GitHub organization login",
                string(&previous, "organization"),
            )
            .await?,
        )
    } else {
        None
    };
    println!(
        "\nThis does not change repository visibility or create a Marketplace listing.\nCrow reviews only repositories you enroll."
    );
    let visibility = choose(
        "Where can this app be installed?",
        string(&previous, "visibility"),
        &[
            ("public", "Personal and organization accounts"),
            ("private", "Only the app owner's account"),
        ],
    )
    .await?;
    config["appRegistration"] = json!({"ownerType":owner_type,"visibility":visibility});
    if let Some(organization) = organization {
        config["appRegistration"]["organization"] = json!(organization);
    }
    let action = registration_action(&config["appRegistration"], &state)?;
    config::save(root, config)?;
    let name = ask(
        "GitHub App name",
        &format!(
            "Crow {} {}",
            string(config, "operator"),
            string(&config["worker"], "id")
                .chars()
                .take(6)
                .collect::<String>()
        ),
    )
    .await?;
    let (completed_tx, completed_rx) = oneshot::channel();
    let shared = Arc::new(RegistrationState {
        config: Mutex::new(config.clone()),
        root: root.to_owned(),
        state,
        secret_path: format!("/setup/{secret}"),
        action,
        manifest: app_manifest(config, &name),
        busy: AtomicBool::new(false),
        completed: Mutex::new(Some(completed_tx)),
        client: reqwest::Client::builder()
            .user_agent("Crow")
            .timeout(Duration::from_secs(30))
            .build()?,
        conversion_base: url::Url::parse("https://api.github.com/app-manifests/")?,
    });
    let port = config["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .context("Invalid setup port")?;
    let listener = tokio::net::TcpListener::bind((string(config, "bind"), port)).await?;
    let (stop_tx, stop_rx) = oneshot::channel();
    let router = Router::new()
        .fallback(registration_handler)
        .with_state(shared.clone());
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
    });
    println!(
        "\nOpen this link in your browser to connect GitHub:\n  {}/setup/{secret}\n\nWaiting for GitHub. Keep this terminal open.",
        string(config, "publicUrl")
    );
    let result = tokio::select! {
        completion = tokio::time::timeout(Duration::from_secs(20 * 60), completed_rx) => match completion {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow::anyhow!("GitHub setup callback closed unexpectedly")),
            Err(_) => Err(anyhow::anyhow!("GitHub setup timed out. Run crow setup to continue.")),
        },
        outcome = &mut server => { return Err(anyhow::anyhow!("GitHub setup server stopped unexpectedly: {outcome:?}")); }
        interrupt = operations::interrupted() => { interrupt.and_then(|()| Err(anyhow::anyhow!("Setup interrupted. Run crow setup to continue."))) }
    };
    let _ = stop_tx.send(());
    if tokio::time::timeout(Duration::from_secs(5), &mut server)
        .await
        .is_err()
    {
        server.abort();
        let _ = server.await;
    }
    result?;
    *config = shared.config.lock().await.clone();
    Ok(())
}

pub async fn github_identity(interactive: bool) -> Result<Value> {
    operations::run("gh", &["--version"], false)
        .await
        .context("Install GitHub CLI from https://cli.github.com, then rerun crow setup.")?;
    if operations::run("gh", &["auth", "status", "--hostname", "github.com"], false)
        .await
        .is_err()
    {
        if !interactive {
            bail!("Run gh auth login --hostname github.com --web first.");
        }
        operations::run(
            "gh",
            &[
                "auth",
                "login",
                "--hostname",
                "github.com",
                "--web",
                "--git-protocol",
                "https",
            ],
            true,
        )
        .await?;
    }
    let user: Value = serde_json::from_str(&operations::run("gh", &["api", "user"], false).await?)?;
    let login = user["login"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Invalid GitHub login")?;
    let token =
        operations::run("gh", &["auth", "token", "--hostname", "github.com"], false).await?;
    if token.trim().is_empty() {
        bail!("GitHub CLI returned an empty authentication token");
    }
    Ok(json!({"login":login,"token":token.trim()}))
}

pub fn apply_setup_port(config: &mut Value, port: Option<&Value>) -> Result<()> {
    let Some(port) = port else {
        if config["role"] != "worker" {
            config["serviceUrl"] = json!(format!("http://127.0.0.1:{}", config["port"]));
        }
        return Ok(());
    };
    let number = if let Some(text) = port.as_str() {
        text.parse::<u16>().ok().filter(|p| *p > 0)
    } else {
        port.as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
    }
    .context("Provide a numeric setup port between 1 and 65535")?;
    if config["role"] == "worker" {
        bail!("Worker-only setup has no listening port.");
    }
    if config["port"] != number
        && (!config["app"].is_null() || !string(config, "publicUrl").is_empty())
    {
        bail!(
            "Choose --port before HTTPS or GitHub App onboarding. An existing installation needs an explicit route migration."
        );
    }
    config["port"] = json!(number);
    config["serviceUrl"] = json!(format!("http://127.0.0.1:{number}"));
    Ok(())
}

pub fn validate_setup_role(config: &Value, selected_role: &str, root: &Path) -> Result<()> {
    let removing_service = config["role"] != "worker" && selected_role == "worker";
    let removing_worker = config["role"] == "both" && selected_role == "service";
    if !removing_service && !removing_worker {
        return Ok(());
    }
    let file = root.join("service.sqlite");
    if !file.try_exists()? {
        return Ok(());
    }
    let database =
        rusqlite::Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let assigned: bool = database.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE kind IN ('repos','jobs') AND (?1 OR json_extract(value,'$.worker') = ?2) AND (kind='repos' OR COALESCE(json_extract(value,'$.state'),'') NOT IN ('completed','superseded','cancelled')))",rusqlite::params![removing_service,string(&config["worker"],"id")],|row| row.get(0))?;
    if assigned {
        if removing_service {
            bail!(
                "Cannot switch to worker-only: repositories or unfinished reviews still require this machine's connection service. Set up a fresh worker installation; changing roles does not migrate service data."
            );
        }
        bail!(
            "Cannot switch to service-only: repositories or unfinished reviews still require this machine's local worker. Keep role both. crow pair creates a new worker and does not migrate repositories or saved sessions."
        );
    }
    Ok(())
}

// Keep interruption handling outside each side effect so cleanup also runs when
// systemctl accepted a request but its process was cancelled before replying.
#[async_trait::async_trait]
trait RestartBackend: Send + Sync {
    async fn current_draining(&self) -> Result<Option<bool>>;
    async fn drain(&self) -> Result<()>;
    async fn wait_drained(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn install(&self) -> Result<()>;
    async fn ready(&self) -> Result<()>;
    async fn recover(&self) -> Result<()>;
    async fn resume(&self) -> Result<()>;
    async fn interrupted(&self) -> Result<()>;
}

async fn restart_with_backend(backend: &impl RestartBackend) -> Result<()> {
    let mut owns_drain = false;
    let mut stop_requested = false;
    let apply = async {
        if let Some(already_draining) = backend.current_draining().await? {
            if !already_draining {
                owns_drain = true;
                backend.drain().await?;
            }
            backend.wait_drained().await?;
            stop_requested = true;
            backend.stop().await?;
        }
        backend.install().await?;
        backend.ready().await?;
        Ok(())
    };
    let mut result: Result<()> = tokio::select! {
        result = apply => result,
        interrupt = backend.interrupted() => interrupt.and_then(|()| Err(anyhow::anyhow!("Setup interrupted. Run crow setup to continue."))),
    };
    if result.is_err()
        && stop_requested
        && let Err(recovery) = backend.recover().await
    {
        result = Err(anyhow::anyhow!(
            "{}. Could not restart Crow after the stop request: {recovery:#}",
            result.unwrap_err()
        ));
    }
    if owns_drain && let Err(cleanup) = backend.resume().await {
        result = Err(anyhow::anyhow!(
            "{}Could not clear the setup-owned drain: {cleanup:#}. Run crow undrain after restoring the service.",
            result
                .as_ref()
                .err()
                .map(|e| format!("{e:#}. "))
                .unwrap_or_default()
        ));
    }
    result
}

struct ServiceRestart<'a> {
    root: &'a Path,
    config: &'a Value,
}

#[async_trait::async_trait]
impl RestartBackend for ServiceRestart<'_> {
    async fn current_draining(&self) -> Result<Option<bool>> {
        Ok(operations::admin(self.config, "status", &Value::Null)
            .await
            .ok()
            .map(|status| status["draining"] == true))
    }
    async fn drain(&self) -> Result<()> {
        println!("Waiting for active reviews before applying setup changes.");
        operations::admin(self.config, "drain", &json!({})).await?;
        Ok(())
    }
    async fn wait_drained(&self) -> Result<()> {
        loop {
            let state = operations::admin(self.config, "status", &Value::Null).await?;
            if !state["jobs"]
                .as_array()
                .is_some_and(|jobs| jobs.iter().any(|job| job["state"] == "reviewing"))
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    async fn stop(&self) -> Result<()> {
        operations::service_action(self.root, "stop").await
    }
    async fn install(&self) -> Result<()> {
        operations::install_service(self.root).await
    }
    async fn ready(&self) -> Result<()> {
        operations::wait_for_service(self.config).await?;
        Ok(())
    }
    async fn recover(&self) -> Result<()> {
        operations::service_action(self.root, "start").await?;
        self.ready().await
    }
    async fn resume(&self) -> Result<()> {
        operations::clear_service_drain(self.root, self.config).await
    }
    async fn interrupted(&self) -> Result<()> {
        operations::interrupted().await
    }
}

pub async fn restart_setup_service(config: &Value, root: &Path) -> Result<()> {
    restart_with_backend(&ServiceRestart { root, config }).await
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn systemd_quote(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        bail!("Invalid character in systemd argument");
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}

async fn install_tunnel_service(config: &Value) -> Result<()> {
    if config["ingress"]["type"] != "cloudflare" {
        return Ok(());
    }
    let file = config["ingress"]["file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Cloudflare tunnel configuration file is missing")?;
    let executable = operations::run("which", &["cloudflared"], false).await?;
    let executable = executable.trim();
    if !Path::new(executable).is_absolute() {
        bail!("Cannot locate cloudflared");
    }
    let id = string(&config["worker"], "id");
    let name = format!(
        "crow-tunnel-{}.service",
        id.chars().take(12).collect::<String>()
    );
    let body = format!(
        "[Unit]\nDescription=Crow Cloudflare Tunnel\nAfter=network-online.target\n\n[Service]\nExecStart={} tunnel --config {} run\nRestart=on-failure\nRestartSec=5\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(executable)?,
        systemd_quote(file)?
    );
    util::atomic_bytes(
        &user_home()?.join(".config/systemd/user").join(&name),
        body.as_bytes(),
    )?;
    operations::run("systemctl", &["--user", "daemon-reload"], false).await?;
    operations::run("systemctl", &["--user", "enable", "--now", &name], false).await?;
    Ok(())
}

struct WorkerRestart<'a> {
    root: &'a Path,
    previous: Option<operations::WorkerDrainState>,
}

#[async_trait::async_trait]
impl RestartBackend for WorkerRestart<'_> {
    async fn current_draining(&self) -> Result<Option<bool>> {
        Ok(self.previous.as_ref().map(|state| state.already_draining))
    }
    async fn drain(&self) -> Result<()> {
        operations::drain_worker(self.root).await
    }
    async fn wait_drained(&self) -> Result<()> {
        // A worker can report draining while reviews are still active. Our own
        // drain call already waits; an earlier caller's request still needs it.
        if self
            .previous
            .as_ref()
            .is_some_and(|state| state.already_draining)
        {
            operations::drain_worker(self.root).await?;
        }
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        operations::service_action(self.root, "stop").await
    }
    async fn install(&self) -> Result<()> {
        operations::install_service(self.root).await
    }
    async fn ready(&self) -> Result<()> {
        operations::wait_for_startup(self.root, env!("CARGO_PKG_VERSION")).await
    }
    async fn recover(&self) -> Result<()> {
        operations::service_action(self.root, "start").await?;
        self.ready().await
    }
    async fn resume(&self) -> Result<()> {
        if let Some(previous) = &self.previous {
            operations::resume_worker(self.root, previous).await?;
        }
        Ok(())
    }
    async fn interrupted(&self) -> Result<()> {
        operations::interrupted().await
    }
}

async fn restart_worker(root: &Path) -> Result<()> {
    restart_with_backend(&WorkerRestart {
        root,
        previous: operations::worker_drain_state(root)?,
    })
    .await
}

pub async fn validate_models(config: &Value, root: &Path, worker: &Value) -> Result<()> {
    if config["role"] == "service" {
        return Ok(());
    }
    let catalog = crate::provider::discover(&config["worker"], root).await?;
    if let Some(warning) = catalog["warning"].as_str() {
        eprintln!("{warning}");
    }
    let models = catalog["models"]
        .as_array()
        .context("The provider returned no model catalog")?;
    crate::provider::validate_model_selection(models, worker, "Review")?;
    if worker["subagents"]["mode"] == "configured" {
        crate::provider::validate_model_selection(models, &worker["subagents"], "Subagent")?;
    }
    Ok(())
}

// Run before ingress creation so an occupied default port does not leave a
// saved external route that then prevents the user selecting another port.
async fn preflight_listener(config: &Value) -> Result<()> {
    if config["role"] == "worker" {
        return Ok(());
    }
    let bind = string(config, "bind");
    let port = config["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .context("Invalid setup port")?;
    let error = match tokio::net::TcpListener::bind((bind, port)).await {
        Ok(listener) => {
            drop(listener);
            return Ok(());
        }
        Err(error) => error,
    };
    if error.kind() == io::ErrorKind::AddrInUse
        && !config["app"].is_null()
        && !string(config, "adminToken").is_empty()
    {
        // A public health response cannot prove this is our installation.
        // Check its local identity and this installation's saved administrator
        // credential, never an unrelated configured remote service URL.
        let host = match bind {
            "0.0.0.0" => "127.0.0.1",
            "::" => "::1",
            other => other,
        };
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let base = format!("http://{host}:{port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let health = client.get(format!("{base}/health")).send().await.ok();
        let owns_service = if let Some(response) = health {
            response.status().is_success()
                && response
                    .json::<Value>()
                    .await
                    .is_ok_and(|body| body["service"] == "crow" && body["configured"] == true)
        } else {
            false
        };
        if owns_service {
            // Health identifies Crow; the authenticated status call proves
            // that this installation owns the administrator credential.
            let mut local = config.clone();
            local["serviceUrl"] = json!(base);
            if tokio::time::timeout(
                Duration::from_secs(5),
                operations::admin(&local, "status", &Value::Null),
            )
            .await
            .is_ok_and(|result| result.is_ok())
            {
                return Ok(());
            }
        }
    }
    bail!(
        "Crow cannot listen on {bind}:{port}: {error}. Stop the other listener or, before HTTPS/App onboarding, rerun `crow setup --port PORT` with an unused port. No new ingress or GitHub connection was created."
    );
}

async fn preflight_systemd(command: &str) -> Result<()> {
    if !cfg!(target_os = "linux") || unsafe { libc::geteuid() } == 0 {
        bail!(
            "Crow setup requires Linux and a normal user session with systemd. Log in as the account that will run Crow, without sudo, and rerun crow setup."
        );
    }
    // Do not print the manager's environment; it can contain private values.
    operations::run(command, &["--user", "show-environment"], false).await.map(|_| ()).map_err(|_| anyhow::anyhow!("Crow could not reach the systemd user manager. Log in directly as the normal user that will run Crow, without sudo. On Ubuntu/Debian, install dbus-user-session if missing, then log out and back in. Check `systemctl --user show-environment` and rerun crow setup. Guided persistent startup requires systemd."))
}

pub async fn setup(root: &Path, options: &Value) -> Result<()> {
    util::private_dir(root)?;
    let _setup_lock = util::acquire_lock(&root.join("setup.lock"))
        .context("Another setup may be running. Finish it before starting setup again.")?;
    let mut config = match util::read_json(&root.join("config.json"))? {
        Some(value) if !value.is_null() => config::load(root)?,
        _ => config::defaults(root),
    };
    println!(
        "\nCrow setup\n\nConnect GitHub, choose a review model, and start Crow in the background.\nPress Enter to accept the value in brackets. You can rerun crow setup to continue."
    );
    let role = match options["role"].as_str() {
        Some(role) => role.to_owned(),
        None => {
            choose(
                "What should run on this machine?",
                string(&config, "role"),
                &[
                    (
                        "both",
                        "Receive GitHub events and run reviews. Recommended.",
                    ),
                    (
                        "service",
                        "Receive GitHub events; send reviews to another machine.",
                    ),
                    (
                        "worker",
                        "Run reviews for a Crow service on another machine.",
                    ),
                ],
            )
            .await?
        }
    };
    if !matches!(role.as_str(), "both" | "service" | "worker") {
        bail!("Choose both, service, or worker");
    }
    validate_setup_role(&config, &role, root)?;
    crate::install::install_downloaded(root).await?;
    println!(
        "\nCrow installed. Settings and review history: {}",
        root.display()
    );
    let local_bin = crate::install::bin_directory()?;
    if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|path| path == local_bin)
    {
        println!(
            "\nTo use the crow command in this terminal, run:\n  export PATH='{}':\"$PATH\"",
            local_bin.to_string_lossy().replace('\'', "'\"'\"'")
        );
    }
    config["role"] = json!(role);
    apply_setup_port(&mut config, options.get("port").filter(|v| !v.is_null()))?;
    config::save(root, &config)?;
    preflight_listener(&config).await?;
    ensure_runtime_prerequisites(&mut config, &role, root).await?;
    preflight_systemd("systemctl").await?;
    let mut identity = Value::Null;
    if role == "worker" {
        println!(
            "\nConnect this worker\nRun crow pair on your service machine, then enter its connection details."
        );
        let previous = string(&config, "serviceUrl");
        let service_url = util::https_url(
            &ask(
                "Connection-service HTTPS URL",
                if previous.starts_with("https:") {
                    previous
                } else {
                    ""
                },
            )
            .await?,
        )?;
        let worker_id = ask("Worker ID from crow pair", string(&config["worker"], "id")).await?;
        let token = if let Some(saved) = saved_pairing_token(&config, &service_url, &worker_id) {
            match operations::check_worker_pairing(&config).await {
                Ok(_) => {
                    println!("Saved worker pairing verified.");
                    saved.to_owned()
                }
                Err(error) => {
                    println!("Could not verify saved pairing: {error}");
                    let entered = ask(
                        "Worker token from crow pair (Enter keeps the saved token)",
                        "",
                    )
                    .await?;
                    if entered.is_empty() {
                        saved.to_owned()
                    } else {
                        entered
                    }
                }
            }
        } else {
            ask("Worker token from crow pair", "").await?
        };
        if token.is_empty() {
            bail!("Worker pairing token required");
        }
        config["serviceUrl"] = json!(service_url);
        config["worker"]["id"] = json!(worker_id);
        config["worker"]["token"] = json!(token);
        config::save(root, &config)?;
    } else {
        println!("\nConnect GitHub");
        ensure_command("gh").await?;
        identity = github_identity(true).await?;
        let operator = string(&config, "operator");
        if !operator.is_empty() && !operator.eq_ignore_ascii_case(string(&identity, "login")) {
            bail!(
                "GitHub CLI is signed in as {}; this installation belongs to {operator}.",
                string(&identity, "login")
            );
        }
        config["operator"] = identity["login"].clone();
        config::save(root, &config)?;
        if string(&config, "publicUrl").is_empty() || config["ingress"]["pending"] == true {
            let ingress = if config["ingress"]["pending"] == true {
                string(&config["ingress"], "type").to_owned()
            } else if let Some(ingress) = options["ingress"].as_str() {
                ingress.to_owned()
            } else {
                println!(
                    "\nGitHub needs a public HTTPS address to send pull request events to Crow."
                );
                choose(
                    "How should GitHub reach this machine?",
                    string(&config["ingress"], "type"),
                    &[
                        ("funnel", "Tailscale Funnel. No domain needed."),
                        (
                            "cloudflare",
                            "Cloudflare Tunnel. Use a domain in your account.",
                        ),
                        ("existing", "Use an HTTPS address you already manage."),
                    ],
                )
                .await?
            };
            match ingress.as_str() {
                "funnel" => {
                    ensure_command("tailscale").await?;
                    configure_funnel(&mut config, root).await?;
                }
                "cloudflare" => {
                    ensure_command("cloudflared").await?;
                    let hostname =
                        ask("Cloudflare hostname, for example connect.example.com", "").await?;
                    configure_cloudflare(&mut config, root, &hostname).await?;
                }
                "existing" => {
                    config["publicUrl"] = json!(util::https_url(
                        &ask(
                            "Public HTTPS address, for example https://crow.example.com",
                            ""
                        )
                        .await?
                    )?);
                    config["ingress"] = json!({"type":"existing"});
                }
                _ => bail!("Choose funnel, cloudflare, or existing"),
            }
            config::save(root, &config)?;
        }
        if config["ingress"]["type"] == "cloudflare" {
            println!("Cloudflare tunnel startup is required for the browser callback.");
            install_tunnel_service(&config).await?;
        }
        if config["app"].is_null() {
            let connection = if config["pendingApp"].is_object() {
                "existing".to_owned()
            } else {
                choose(
                    "Connect a GitHub App",
                    "create",
                    &[
                        ("create", "Create a new GitHub App."),
                        ("existing", "Connect an App you already manage."),
                    ],
                )
                .await?
            };
            if connection == "existing" {
                connect_existing_app(&mut config, root).await?;
            } else {
                register_app(&mut config, root).await?;
            }
        }
        if config["app"].is_null() {
            bail!("GitHub App registration did not complete");
        }
        println!(
            "\nInstall the app and choose repositories in your browser:\n  https://github.com/apps/{}/installations/new",
            string(&config["app"], "slug")
        );
        ask("Press Enter when the App is installed", "").await?;
        config["app"]["botId"] = json!(
            crate::github::GitHub::new(Some(config["app"].clone()))
                .bot_id()
                .await?
        );
        config::save(root, &config)?;
    }
    if role != "service" {
        setup_provider(&mut config, root).await?;
    }
    if role != "worker" {
        restart_setup_service(&config, root).await?;
    } else {
        restart_worker(root).await?;
    }
    if role != "worker" {
        let current = operations::admin(&config, "status", &Value::Null).await?;
        let names = if role == "service" {
            println!(
                "Pair a worker with crow pair, then enroll repositories using crow enroll owner/repo --worker ID."
            );
            String::new()
        } else {
            println!(
                "\nChoose repositories for automatic reviews\nEnter GitHub names such as owner/api, owner/website.\nOnly pull requests from your account will be reviewed by default.\nPress Enter to keep your current selection. Add repositories later with crow enroll."
            );
            ask("Repositories", "").await?
        };
        let mut enrolled: HashSet<String> = current["repos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|repo| repo["name"].as_str())
            .map(str::to_ascii_lowercase)
            .collect();
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if !enrolled.insert(name.to_ascii_lowercase()) {
                continue;
            }
            let result = operations::admin(&config,"enroll",&json!({"repo":name,"githubToken":identity["token"],"worker":config["worker"]["id"],"policy":"selected","authors":[config["operator"]],"includeBacklog":false})).await?;
            println!("{}", crate::output::render("enroll", &result));
        }
    }
    let result = operations::doctor(&config, root, true).await?;
    println!("{}", crate::output::render("doctor", &result));
    if result["ok"] != true {
        bail!("Some setup checks failed. Fix the reported issue and rerun crow setup.");
    }
    println!(
        "\nSetup complete. Crow is running in the background.\n\n  crow status     See connected repositories and review activity\n  crow doctor     Check your installation\n\nNo test review was started."
    );
    println!(
        "{}",
        match role.as_str() {
            "worker" => "This worker will process reviews sent by your Crow service.",
            "service" => "Pair a worker and enroll repositories to start reviews.",
            _ => "New eligible pull requests in enrolled repositories will trigger reviews.",
        }
    );
    Ok(())
}

fn saved_pairing_token<'a>(
    config: &'a Value,
    service_url: &str,
    worker_id: &str,
) -> Option<&'a str> {
    let saved_url = util::https_url(string(config, "serviceUrl")).ok()?;
    let same_service = saved_url == service_url;
    (same_service && string(&config["worker"], "id") == worker_id)
        .then(|| string(&config["worker"], "token"))
        .filter(|token| !token.is_empty())
}

fn setup_model_default<'a>(models: &'a [Value], previous: &str) -> Result<&'a str> {
    models.iter().find(|model| !previous.is_empty() && model["model"] == previous)
        .or_else(|| models.iter().find(|model| model["isDefault"] == true))
        .and_then(|model| model["model"].as_str())
        .context("The provider did not report a usable default model. Retry model discovery before completing setup.")
}

fn setup_effort_default<'a>(model: &'a Value, previous: &str) -> Result<&'a str> {
    let efforts = model["supportedReasoningEfforts"]
        .as_array()
        .context("The selected model did not report reasoning levels")?;
    efforts
        .iter()
        .find(|effort| !previous.is_empty() && effort["reasoningEffort"] == previous)
        .or_else(|| {
            efforts
                .iter()
                .find(|effort| effort["reasoningEffort"] == model["defaultReasoningEffort"])
        })
        .and_then(|effort| effort["reasoningEffort"].as_str())
        .context("The selected model did not report a supported default reasoning level")
}

async fn setup_provider(config: &mut Value, root: &Path) -> Result<()> {
    println!("\nChoose how Crow reviews code");
    ensure_command("git").await?;
    if operations::run(string(&config["worker"], "codex"), &["--version"], false)
        .await
        .is_err()
    {
        if !confirm(
            "Codex is missing. Install the latest official standalone Codex package?",
            true,
        )
        .await?
        {
            bail!("Install Codex and rerun setup.");
        }
        config["worker"]["codex"] = json!(crate::install::install_codex(root).await?);
        config::save(root, config)?;
    }
    ensure_codex_capabilities(config, root).await?;
    if crate::provider::auth_status(&config["worker"], root).await?["authenticated"] != true {
        println!("Authenticate on your desktop using the URL and code printed below.");
        crate::provider::login(&config["worker"], root).await?;
    }
    let catalog = crate::provider::discover(&config["worker"], root).await?;
    if let Some(warning) = catalog["warning"].as_str() {
        println!("{warning}");
    }
    let models = catalog["models"]
        .as_array()
        .context("The provider returned no model catalog")?;
    let initial = setup_model_default(models, string(&config["worker"], "model"))?;
    let model_choices: Vec<_> = models
        .iter()
        .map(|model| {
            (
                model["model"].as_str().unwrap_or(string(model, "id")),
                model["displayName"]
                    .as_str()
                    .unwrap_or(string(model, "model")),
            )
        })
        .collect();
    let selected = choose("Review model", initial, &model_choices).await?;
    let model = models
        .iter()
        .find(|model| model["model"] == selected)
        .context("Select a model from the provider list.")?;
    config["worker"]["model"] = json!(selected);
    let efforts = model["supportedReasoningEfforts"]
        .as_array()
        .context("The selected model did not report reasoning levels")?;
    let previous = string(&config["worker"], "effort");
    println!("\nHigher reasoning levels give the model more time to work on each review.");
    let effort_choices: Vec<_> = efforts
        .iter()
        .filter_map(|effort| {
            effort["reasoningEffort"]
                .as_str()
                .map(|name| (name, string(effort, "description")))
        })
        .collect();
    let effort = choose(
        "Reasoning level",
        setup_effort_default(model, previous)?,
        &effort_choices,
    )
    .await?;
    if !efforts.iter().any(|e| e["reasoningEffort"] == effort) {
        bail!("Unsupported reasoning level for this model.");
    }
    config["worker"]["effort"] = json!(effort);
    config::save(root, config)?;
    Ok(())
}

/// A locally installed Codex can answer `--version` while lacking the JSON,
/// resume, or isolation controls Crow needs. Offer Crow's verified standalone
/// binary as an explicit repair without replacing a user's executable silently.
async fn ensure_codex_capabilities(config: &mut Value, root: &Path) -> Result<()> {
    let diagnostics = crate::provider::diagnostics(&config["worker"], root, false).await?;
    let failed = diagnostics["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|check| check["ok"] != true)
        .collect::<Vec<_>>();
    if failed.is_empty() {
        return Ok(());
    }
    println!("\nThe configured Codex executable does not provide all controls Crow requires:");
    for check in &failed {
        println!("  {}: {}", string(check, "name"), string(check, "detail"));
    }
    if !confirm(
        "Install Crow's verified standalone Codex executable instead?",
        true,
    )
    .await?
    {
        bail!("Install a current official Codex CLI and rerun setup.");
    }
    config["worker"]["codex"] = json!(crate::install::install_codex(root).await?);
    config::save(root, config)?;
    let repaired = crate::provider::diagnostics(&config["worker"], root, false).await?;
    ensure!(
        repaired["ok"] == true,
        "The installed Codex executable still lacks required controls. Rerun setup after updating Codex."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_preflight_rejects_foreign_ports_before_route_creation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = json!({"role":"both", "bind":"127.0.0.1", "port":port});
        let error = preflight_listener(&config).await.unwrap_err().to_string();
        assert!(error.contains("crow setup --port PORT"), "{error}");
        assert!(config["publicUrl"].is_null());
        config["role"] = json!("worker");
        preflight_listener(&config).await.unwrap();
        config["role"] = json!("service");
        // Let the OS select the free port within the bind itself. Reusing a
        // released ephemeral port races other tests and subprocesses.
        config["port"] = json!(0);
        preflight_listener(&config).await.unwrap();
    }

    #[tokio::test]
    async fn listener_preflight_allows_only_authenticated_existing_service() {
        use axum::{Json, http::HeaderMap, routing::get};
        let router = Router::new()
            .route(
                "/health",
                get(|| async { Json(json!({"service":"crow","configured":true})) }),
            )
            .route(
                "/admin/status",
                get(|headers: HeaderMap| async move {
                    if headers
                        .get("authorization")
                        .and_then(|header| header.to_str().ok())
                        == Some("Bearer installation-secret")
                    {
                        (StatusCode::OK, Json(json!({"jobs":[],"repos":[]})))
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":"Unauthorized"})),
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut config = json!({"role":"both","bind":"127.0.0.1","port":port,"app":{"id":1},"adminToken":"installation-secret","serviceUrl":"https://must-not-contact.invalid"});
        preflight_listener(&config).await.unwrap();
        config["adminToken"] = json!("wrong-installation");
        assert!(preflight_listener(&config).await.is_err());
        server.abort();
        let _ = server.await;

        // A foreign server returning plausible status JSON without Crow's health
        // identity cannot exempt its occupied port from the preflight.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config["port"] = json!(listener.local_addr().unwrap().port());
        let router = Router::new().route(
            "/admin/status",
            get(|| async { Json(json!({"jobs":[],"repos":[]})) }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        assert!(preflight_listener(&config).await.is_err());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn systemd_preflight_reports_actionable_errors_without_environment_output() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("systemctl");
        for code in [0, 1] {
            std::fs::write(&command, format!("#!/bin/sh\ntest \"$*\" = '--user show-environment' || exit 9\nprintf 'PRIVATE_MANAGER_VALUE=canary\\n'\nexit {code}\n")).unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = preflight_systemd(command.to_str().unwrap()).await;
            if code == 0 && unsafe { libc::geteuid() } != 0 {
                result.unwrap();
            } else {
                let message = result.unwrap_err().to_string();
                assert!(message.contains("normal user"), "{message}");
                assert!(!message.contains("PRIVATE_MANAGER_VALUE"), "{message}");
            }
        }
    }

    #[test]
    fn saved_pairing_is_reused_only_for_the_same_service_and_worker() {
        let config = json!({"serviceUrl":"https://service.example/","worker":{"id":"worker-one","token":"saved-secret"}});
        assert_eq!(
            saved_pairing_token(
                &config,
                &util::https_url("https://service.example/").unwrap(),
                "worker-one"
            ),
            Some("saved-secret")
        );
        assert_eq!(
            saved_pairing_token(&config, "https://other.example", "worker-one"),
            None
        );
        assert_eq!(
            saved_pairing_token(&config, "https://service.example", "worker-two"),
            None
        );
        assert_eq!(
            saved_pairing_token(
                &json!({"serviceUrl":"https://service.example","worker":{"id":"worker-one","token":""}}),
                "https://service.example",
                "worker-one"
            ),
            None
        );
        assert_eq!(
            saved_pairing_token(
                &json!({"serviceUrl":"http://service.example","worker":{"id":"worker-one","token":"saved-secret"}}),
                "https://service.example",
                "worker-one"
            ),
            None
        );
    }

    #[test]
    fn provider_prompt_defaults_never_offer_removed_models_or_unsupported_effort() {
        let models = vec![
            json!({"model":"current-default","isDefault":true,"defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]}),
            json!({"model":"saved-model","isDefault":false,"defaultReasoningEffort":"low","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}),
        ];
        assert_eq!(
            setup_model_default(&models, "saved-model").unwrap(),
            "saved-model"
        );
        assert_eq!(
            setup_model_default(&models, "removed-model").unwrap(),
            "current-default"
        );
        assert_eq!(setup_model_default(&models, "").unwrap(), "current-default");
        assert_eq!(setup_effort_default(&models[0], "high").unwrap(), "high");
        assert_eq!(setup_effort_default(&models[1], "high").unwrap(), "low");
        assert_eq!(
            setup_effort_default(&models[0], "removed-effort").unwrap(),
            "medium"
        );
        assert!(setup_model_default(&[], "removed-model").is_err());
        assert!(setup_effort_default(&json!({"defaultReasoningEffort":"high","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}), "").is_err());
    }

    struct AppConnectionProbe {
        root: PathBuf,
        fail: bool,
        calls: std::sync::Mutex<Vec<Value>>,
    }
    #[async_trait::async_trait]
    impl GitHubApi for AppConnectionProbe {
        async fn request(
            &self,
            path: &str,
            _: Option<&str>,
            method: &str,
            body: Option<&Value>,
        ) -> Result<Value> {
            assert_eq!(path, "/app/hook/config");
            if method == "PATCH" {
                let saved = config::load(&self.root)?;
                assert!(saved["app"].is_null());
                assert_eq!(
                    body.unwrap()["secret"],
                    saved["pendingApp"]["app"]["webhookSecret"]
                );
                self.calls.lock().unwrap().push(body.unwrap().clone());
                if self.fail {
                    bail!("injected timeout after GitHub accepts the change");
                }
            }
            Ok(
                json!({"url":"https://crow.example/webhooks/github","content_type":"json","insecure_ssl":"0"}),
            )
        }
    }

    fn pending_app_config(root: &Path) -> Value {
        let mut config = config::defaults(root);
        config["publicUrl"] = json!("https://crow.example");
        config["pendingApp"] = json!({"app":{"id":42,"pem":"private-key","slug":"my-crow","webhookSecret":"saved-secret","botId":null},"webhookUrl":"https://crow.example/webhooks/github"});
        config
    }

    #[tokio::test]
    async fn existing_app_connection_recovers_with_the_same_credentials_after_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut config = pending_app_config(root.path());
        let mut probe = AppConnectionProbe {
            root: root.path().to_owned(),
            fail: true,
            calls: Default::default(),
        };
        assert!(
            finish_existing_app(&mut config, root.path(), &probe, "jwt")
                .await
                .is_err()
        );
        let saved = config::load(root.path()).unwrap();
        assert!(saved["app"].is_null());
        assert_eq!(
            std::fs::metadata(root.path().join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        probe.fail = false;
        let mut resumed = saved.clone();
        finish_existing_app(&mut resumed, root.path(), &probe, "jwt")
            .await
            .unwrap();
        assert_eq!(resumed["app"], saved["pendingApp"]["app"]);
        assert!(resumed.get("pendingApp").is_none());
        assert_eq!(config::load(root.path()).unwrap(), resumed);
        let calls = probe.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
    }

    #[tokio::test]
    async fn existing_app_connection_refuses_replacement_and_destination_changes() {
        let root = tempfile::tempdir().unwrap();
        let mut config = pending_app_config(root.path());
        let probe = AppConnectionProbe {
            root: root.path().to_owned(),
            fail: false,
            calls: Default::default(),
        };
        config["app"] = config["pendingApp"]["app"].clone();
        assert!(
            finish_existing_app(&mut config, root.path(), &probe, "jwt")
                .await
                .is_err()
        );
        config["app"] = Value::Null;
        config["publicUrl"] = json!("https://different.example");
        assert!(
            finish_existing_app(&mut config, root.path(), &probe, "jwt")
                .await
                .is_err()
        );
        assert!(probe.calls.lock().unwrap().is_empty());
        assert!(!root.path().join("config.json").exists());
    }

    #[test]
    fn existing_app_requirements_and_webhook_are_checked() {
        let app = json!({"permissions":{"contents":"read","pull_requests":"write","issues":"write"},"events":["pull_request","push","issue_comment"]});
        validate_app_requirements(&app).unwrap();
        for permission in ["contents", "pull_requests", "issues"] {
            let mut invalid = app.clone();
            invalid["permissions"][permission] = json!("none");
            assert!(validate_app_requirements(&invalid).is_err());
        }
        for event in ["pull_request", "push", "issue_comment"] {
            let mut invalid = app.clone();
            invalid["events"]
                .as_array_mut()
                .unwrap()
                .retain(|e| e != event);
            assert!(validate_app_requirements(&invalid).is_err());
        }
        let mut broader = app;
        broader["permissions"]["contents"] = json!("write");
        validate_app_requirements(&broader).unwrap();
        let expected = "https://crow.example/webhooks/github";
        let hook = json!({"url":expected,"content_type":"json","insecure_ssl":"0"});
        validate_app_webhook(&hook, expected).unwrap();
        for (key, value) in [
            ("url", "https://old.example/webhooks/github"),
            ("content_type", "form"),
            ("insecure_ssl", "1"),
        ] {
            let mut invalid = hook.clone();
            invalid[key] = json!(value);
            assert!(validate_app_webhook(&invalid, expected).is_err());
        }
    }

    #[test]
    fn prompt_subprocess() {
        let Some(directory) = std::env::var_os("CROW_PROMPT_TEST_DIRECTORY") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // Registration previously polled and then dropped this listener.
            assert!(tokio::time::timeout(Duration::from_millis(10), operations::interrupted()).await.is_err());
            if std::env::var("CROW_PROMPT_TEST_MODE").unwrap() == "answers" {
                let first = ask("First", "default").await.unwrap();
                assert_eq!(unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) }, flags);
                let second = ask("Second", "default").await.unwrap();
                assert_eq!(unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) }, flags);
                let third = ask("Third", "default");
                tokio::pin!(third);
                tokio::select! {
                    result = &mut third => panic!("Partial UTF-8 input completed prematurely: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                }
                util::atomic(&directory.join("ready.json"), &json!([first, second])).unwrap();
                assert_eq!(third.await.unwrap(), "尾");
                assert_eq!(unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) }, flags);
                assert!(ask("EOF", "default").await.unwrap_err().to_string().contains("interactive input"));
            } else {
                let prompt = ask("Waiting for input", "default");
                tokio::pin!(prompt);
                tokio::select! {
                    result = &mut prompt => panic!("Prompt completed before its signal: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                }
                util::atomic(&directory.join("ready.json"), &json!(true)).unwrap();
                assert!(prompt.await.unwrap_err().to_string().contains("Setup interrupted"));
            }
            assert_eq!(unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) }, flags);
        });
        // This must also finish. Cancelling a spawn_blocking stdin reader would
        // return from ask but leave runtime destruction waiting for stdin EOF.
        drop(runtime);
    }

    async fn prompt_child(mode: &str, signal: Option<i32>) {
        use tokio::io::AsyncWriteExt;
        let directory = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "setup::tests::prompt_subprocess", "--nocapture"])
            .env("CROW_PROMPT_TEST_DIRECTORY", directory.path())
            .env("CROW_PROMPT_TEST_MODE", mode)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Child::wait closes its own stdin; retain a separate handle to prove
        // signal cancellation also shuts down the runtime without stdin EOF.
        let mut input = child.stdin.take();
        let test = async {
            input
                .as_mut()
                .unwrap()
                .write_all(if mode == "answers" {
                    b" r\xc3\xa9view \n\n\xe5"
                } else {
                    b"partial input"
                })
                .await?;
            while !directory.path().join("ready.json").exists() {
                anyhow::ensure!(
                    child.try_wait()?.is_none(),
                    "Prompt subprocess exited before becoming ready"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if let Some(signal) = signal {
                anyhow::ensure!(
                    unsafe { libc::kill(child.id().unwrap() as i32, signal) } == 0,
                    "Could not signal prompt subprocess"
                );
                // Keep stdin open until the child and its Tokio runtime exit.
            } else {
                anyhow::ensure!(
                    util::read_json(&directory.path().join("ready.json"))?
                        == Some(json!(["réview", "default"])),
                    "Prompt answers did not preserve defaults and UTF-8"
                );
                input.as_mut().unwrap().write_all(b"\xb0\xbe").await?;
                drop(input.take());
            }
            Ok::<_, anyhow::Error>(child.wait().await?)
        };
        match tokio::time::timeout(Duration::from_secs(5), test).await {
            Ok(Ok(status)) => assert!(status.success(), "Prompt subprocess failed: {status}"),
            error => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                panic!(
                    "Prompt subprocess failed or did not exit while stdin remained open: {error:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn prompts_exit_on_signals_after_registration_without_stdin_eof() {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            prompt_child("signal", Some(signal)).await;
        }
    }

    #[tokio::test]
    async fn prompts_preserve_queued_answers_partial_utf8_defaults_and_eof() {
        prompt_child("answers", None).await;
    }

    struct RestartProbe {
        calls: std::sync::Mutex<Vec<&'static str>>,
        already_draining: bool,
        fail_at: Option<&'static str>,
        interrupt_at: Option<&'static str>,
        interruption: tokio::sync::Notify,
    }

    impl RestartProbe {
        fn new(
            already_draining: bool,
            fail_at: Option<&'static str>,
            interrupt_at: Option<&'static str>,
        ) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                already_draining,
                fail_at,
                interrupt_at,
                interruption: tokio::sync::Notify::new(),
            }
        }
        async fn step(&self, name: &'static str) -> Result<()> {
            self.calls.lock().unwrap().push(name);
            if self.interrupt_at == Some(name) {
                self.interruption.notify_one();
                std::future::pending::<()>().await;
            }
            if self.fail_at == Some(name) {
                bail!("injected {name} failure");
            }
            Ok(())
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl RestartBackend for RestartProbe {
        async fn current_draining(&self) -> Result<Option<bool>> {
            self.step("status").await?;
            Ok(Some(self.already_draining))
        }
        async fn drain(&self) -> Result<()> {
            self.step("drain").await
        }
        async fn wait_drained(&self) -> Result<()> {
            self.step("wait").await
        }
        async fn stop(&self) -> Result<()> {
            self.step("stop").await
        }
        async fn install(&self) -> Result<()> {
            self.step("install").await
        }
        async fn ready(&self) -> Result<()> {
            self.step("ready").await
        }
        async fn recover(&self) -> Result<()> {
            self.step("recover").await
        }
        async fn resume(&self) -> Result<()> {
            self.step("resume").await
        }
        async fn interrupted(&self) -> Result<()> {
            self.interruption.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn interrupted_drain_resumes_without_stopping_active_jobs() {
        for interrupted_step in ["drain", "wait"] {
            let backend = RestartProbe::new(false, None, Some(interrupted_step));
            assert!(restart_with_backend(&backend).await.is_err());
            let calls = backend.calls();
            assert_eq!(calls.last(), Some(&"resume"));
            assert!(!calls.contains(&"stop"));
            assert!(!calls.contains(&"recover"));
            assert!(!calls.contains(&"install"));
        }
    }

    #[tokio::test]
    async fn failed_or_interrupted_stop_recovers_before_resuming_owned_drain() {
        for (failure, interruption) in [
            (Some("stop"), None),
            (None, Some("stop")),
            (Some("install"), None),
            (Some("ready"), None),
        ] {
            let backend = RestartProbe::new(false, failure, interruption);
            assert!(restart_with_backend(&backend).await.is_err());
            let calls = backend.calls();
            assert!(calls.ends_with(&["recover", "resume"]), "{calls:?}");
        }
    }

    #[tokio::test]
    async fn restart_preserves_an_existing_drain_on_success_and_failure() {
        for failure in [None, Some("stop"), Some("ready")] {
            let backend = RestartProbe::new(true, failure, None);
            let result = restart_with_backend(&backend).await;
            assert_eq!(result.is_err(), failure.is_some());
            let calls = backend.calls();
            assert!(!calls.contains(&"drain"));
            assert!(!calls.contains(&"resume"));
        }
    }

    #[tokio::test]
    async fn lost_drain_response_is_cleaned_up_without_stopping() {
        let backend = RestartProbe::new(false, Some("drain"), None);
        assert!(restart_with_backend(&backend).await.is_err());
        assert_eq!(backend.calls(), vec!["status", "drain", "resume"]);
    }
    #[test]
    fn funnel_respects_existing_routes_and_ownership() {
        assert_eq!(choose_funnel_port(&json!({}), None, None).unwrap(), 8443);
        let status = json!({"TCP":{"8443":{}},"Web":{"host:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}});
        assert_eq!(choose_funnel_port(&status, None, None).unwrap(), 10000);
        assert_eq!(
            choose_funnel_port(&status, Some(443), Some("http://127.0.0.1:8080")).unwrap(),
            443
        );
        assert_eq!(
            choose_funnel_port(&status, Some(443), Some("http://127.0.0.1:9999")).unwrap(),
            10000
        );
        let shared = json!({"Web":{"host:443":{"Handlers":{"/":{"Proxy":"ours"},"/other":{"Proxy":"theirs"}}}}});
        assert_eq!(
            choose_funnel_port(&shared, Some(443), Some("ours")).unwrap(),
            8443
        );
        assert!(
            choose_funnel_port(&json!({"TCP":{"443":{},"8443":{},"10000":{}}}), None, None)
                .is_err()
        );
        assert!(choose_funnel_port(&json!({"Web":[]}), None, None).is_err());
    }
    #[test]
    fn manifest_has_only_supported_events_and_correct_scope() {
        let mut config =
            json!({"publicUrl":"https://crow.example","appRegistration":{"visibility":"private"}});
        let manifest = app_manifest(&config, "Crow");
        assert_eq!(manifest["public"], false);
        assert_eq!(
            manifest["hook_attributes"]["url"],
            "https://crow.example/webhooks/github"
        );
        assert_eq!(
            manifest["default_events"],
            json!(["pull_request", "push", "issue_comment"])
        );
        config["appRegistration"]["visibility"] = json!("public");
        assert_eq!(app_manifest(&config, "Crow")["public"], true);
    }
    #[test]
    fn registration_validates_state_and_organization() {
        let state = "a".repeat(64);
        assert_eq!(
            registration_action(&json!({"ownerType":"personal"}), &state).unwrap(),
            format!("https://github.com/settings/apps/new?state={state}")
        );
        assert!(registration_action(&json!({"ownerType":"personal"}), "bad").is_err());
        for org in ["-org", "org-", "a/b", "", "hello world"] {
            assert!(
                registration_action(
                    &json!({"ownerType":"organization","organization":org}),
                    &state
                )
                .is_err()
            );
        }
        assert!(
            registration_action(
                &json!({"ownerType":"organization","organization":"my-org"}),
                &state
            )
            .unwrap()
            .contains("/organizations/my-org/")
        );
    }
    #[test]
    fn setup_port_prevents_implicit_migration() {
        let mut config = json!({"role":"both","port":8080,"app":null,"publicUrl":""});
        apply_setup_port(&mut config, Some(&json!("9999"))).unwrap();
        assert_eq!(config["serviceUrl"], "http://127.0.0.1:9999");
        config["publicUrl"] = json!("https://crow.example");
        assert!(apply_setup_port(&mut config, Some(&json!(8080))).is_err());
        apply_setup_port(&mut config, Some(&json!(9999))).unwrap();
        config["role"] = json!("worker");
        assert!(apply_setup_port(&mut config, Some(&json!(9999))).is_err());
        assert!(apply_setup_port(&mut config, Some(&json!(true))).is_err());
    }
    #[test]
    fn runtime_prerequisites_follow_role_and_explicit_execution_settings() {
        assert!(runtime_requested(
            &json!({"execution":{"automatic":true}}),
            "both"
        ));
        assert!(runtime_requested(
            &json!({"execution":{"repositories":{"owner/repo":{}}}}),
            "worker"
        ));
        assert!(!runtime_requested(
            &json!({"execution":{"automatic":true}}),
            "service"
        ));
        assert!(!runtime_requested(
            &json!({"execution":{"automatic":false,"repositories":{}}}),
            "worker"
        ));
    }

    struct RuntimeSetupProbe {
        checks: std::sync::Mutex<std::collections::VecDeque<bool>>,
        answers: std::sync::Mutex<std::collections::VecDeque<Option<bool>>>,
        installs: std::sync::Mutex<Vec<Vec<String>>>,
        check_count: std::sync::Mutex<usize>,
        commands_present: bool,
        missing_commands: HashSet<&'static str>,
        apt: bool,
        install_fails: bool,
        installed_helpers_present: bool,
    }
    impl RuntimeSetupProbe {
        fn new(checks: &[bool], answers: &[Option<bool>]) -> Self {
            Self {
                checks: std::sync::Mutex::new(checks.iter().copied().collect()),
                answers: std::sync::Mutex::new(answers.iter().copied().collect()),
                installs: std::sync::Mutex::new(Vec::new()),
                check_count: std::sync::Mutex::new(0),
                commands_present: false,
                missing_commands: HashSet::new(),
                apt: true,
                install_fails: false,
                installed_helpers_present: true,
            }
        }
    }
    #[async_trait::async_trait]
    impl RuntimeSetupBackend for RuntimeSetupProbe {
        async fn check(&self, _: &Value) -> Result<()> {
            *self.check_count.lock().unwrap() += 1;
            if self
                .checks
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected check")
            {
                Ok(())
            } else {
                bail!("fixture rootless prerequisite failed")
            }
        }
        fn available(&self, command: &str) -> bool {
            (self.commands_present && !self.missing_commands.contains(command))
                || (!self.installs.lock().unwrap().is_empty()
                    && !self.install_fails
                    && self.installed_helpers_present)
        }
        fn supports_apt(&self) -> bool {
            self.apt
        }
        async fn confirm(&self, _: &str, _: bool) -> Result<bool> {
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected prompt")
                .context("Setup interrupted")
        }
        async fn install(&self, packages: &[String]) -> Result<()> {
            self.installs.lock().unwrap().push(packages.to_vec());
            if self.install_fails {
                bail!("fixture apt failure")
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn runtime_setup_skips_disabled_service_and_already_working_hosts() {
        for (mut settings, role) in [
            (json!({"execution":{"automatic":false}}), "worker"),
            (json!({"execution":{"automatic":true}}), "service"),
        ] {
            let backend = RuntimeSetupProbe::new(&[], &[]);
            runtime_setup_with_backend(&mut settings, role, &backend)
                .await
                .unwrap();
            assert_eq!(*backend.check_count.lock().unwrap(), 0);
            assert!(backend.installs.lock().unwrap().is_empty());
        }
        let mut settings = json!({"execution":{"automatic":true,"podman":"/custom/podman"}});
        let backend = RuntimeSetupProbe::new(&[true], &[]);
        runtime_setup_with_backend(&mut settings, "worker", &backend)
            .await
            .unwrap();
        assert_eq!(*backend.check_count.lock().unwrap(), 1);
        assert!(backend.installs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_setup_installs_missing_packages_once_and_rechecks_rootless_support() {
        let mut settings = json!({"execution":{"automatic":true}});
        let backend = RuntimeSetupProbe::new(&[false, true], &[Some(true)]);
        runtime_setup_with_backend(&mut settings, "both", &backend)
            .await
            .unwrap();
        assert_eq!(*backend.check_count.lock().unwrap(), 2);
        assert_eq!(
            backend.installs.lock().unwrap().as_slice(),
            &[vec![
                "podman",
                "uidmap",
                "slirp4netns",
                "fuse-overlayfs",
                "dbus-user-session"
            ]]
        );
        assert_eq!(settings["execution"]["automatic"], true);
    }

    #[tokio::test]
    async fn runtime_setup_checks_required_helpers_even_when_podman_info_succeeds() {
        let mut settings = json!({"execution":{"automatic":true}});
        let mut backend = RuntimeSetupProbe::new(&[true, true], &[Some(true)]);
        backend.commands_present = true;
        backend.missing_commands = HashSet::from(["newuidmap", "newgidmap"]);
        runtime_setup_with_backend(&mut settings, "worker", &backend)
            .await
            .unwrap();
        assert_eq!(
            backend.installs.lock().unwrap().as_slice(),
            &[vec!["uidmap", "dbus-user-session"]]
        );
        assert_eq!(*backend.check_count.lock().unwrap(), 2);

        let mut backend = RuntimeSetupProbe::new(&[true], &[]);
        backend.commands_present = true;
        backend.missing_commands = HashSet::from(["slirp4netns", "fuse-overlayfs"]);
        runtime_setup_with_backend(&mut settings, "worker", &backend)
            .await
            .unwrap();
        assert!(backend.installs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_setup_rechecks_required_helpers_after_successful_package_install() {
        let mut settings = json!({"execution":{"automatic":true}});
        let mut backend = RuntimeSetupProbe::new(&[true, true], &[Some(true), Some(false)]);
        backend.commands_present = true;
        backend.missing_commands = HashSet::from(["newgidmap"]);
        backend.installed_helpers_present = false;
        let error = runtime_setup_with_backend(&mut settings, "worker", &backend)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("newuidmap or newgidmap is still unavailable"));
        assert_eq!(settings["execution"]["automatic"], true);
    }

    #[tokio::test]
    async fn runtime_setup_decline_or_install_failure_can_explicitly_disable_testing() {
        for install_fails in [false, true] {
            let mut settings = json!({"execution":{"automatic":true,"repositories":{"owner/repo":{}},"podman":"podman"}});
            let mut backend = RuntimeSetupProbe::new(&[false], &[Some(install_fails), Some(true)]);
            backend.install_fails = install_fails;
            runtime_setup_with_backend(&mut settings, "worker", &backend)
                .await
                .unwrap();
            assert_eq!(settings["execution"]["automatic"], false);
            assert_eq!(settings["execution"]["repositories"], json!({}));
            assert_eq!(settings["execution"]["podman"], "podman");
            assert_eq!(
                backend.installs.lock().unwrap().len(),
                usize::from(install_fails)
            );
        }
    }

    #[tokio::test]
    async fn runtime_setup_cancellation_preserves_execution_authority() {
        for commands_present in [false, true] {
            let mut settings = json!({"execution":{"automatic":true}});
            let original = settings.clone();
            let mut backend = RuntimeSetupProbe::new(&[false], &[None]);
            backend.commands_present = commands_present;
            let error = runtime_setup_with_backend(&mut settings, "worker", &backend)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Setup interrupted"));
            assert_eq!(settings, original);
            assert!(backend.installs.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn runtime_setup_does_not_install_over_custom_commands_or_on_unsupported_hosts() {
        for custom in [false, true] {
            let mut settings = json!({"execution":{"automatic":true}});
            if custom {
                settings["execution"]["podman"] = json!("/custom/podman");
            }
            let original = settings.clone();
            let mut backend = RuntimeSetupProbe::new(&[false], &[Some(false)]);
            backend.apt = custom;
            let error = runtime_setup_with_backend(&mut settings, "worker", &backend)
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("fixture rootless prerequisite failed"));
            assert!(error.to_string().contains("Runtime setup is incomplete"));
            assert_eq!(settings, original);
            assert!(backend.installs.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn runtime_setup_does_not_report_success_when_installed_podman_is_unusable() {
        let mut settings = json!({"execution":{"automatic":true}});
        let backend = RuntimeSetupProbe::new(&[false, false], &[Some(true), Some(false)]);
        let error = runtime_setup_with_backend(&mut settings, "worker", &backend)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("fixture rootless prerequisite failed"));
        assert_eq!(*backend.check_count.lock().unwrap(), 2);
        assert_eq!(backend.installs.lock().unwrap().len(), 1);
        assert_eq!(settings["execution"]["automatic"], true);
    }
    #[test]
    fn role_changes_keep_repository_and_unfinished_job_assignments() {
        let directory = tempfile::tempdir().unwrap();
        let config = json!({"role":"both","worker":{"id":"local"}});
        let db = rusqlite::Connection::open(directory.path().join("service.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE records(kind TEXT,value TEXT); INSERT INTO records VALUES('jobs','{\"worker\":\"local\",\"state\":\"queued\"}');").unwrap();
        assert!(validate_setup_role(&config, "service", directory.path()).is_err());
        assert!(validate_setup_role(&config, "worker", directory.path()).is_err());
        db.execute(
            "UPDATE records SET value=?",
            [json!({"worker":"local","state":"completed"}).to_string()],
        )
        .unwrap();
        validate_setup_role(&config, "service", directory.path()).unwrap();
        db.execute(
            "INSERT INTO records VALUES('repos',?)",
            [json!({"worker":"remote"}).to_string()],
        )
        .unwrap();
        validate_setup_role(&config, "service", directory.path()).unwrap();
        assert!(validate_setup_role(&config, "worker", directory.path()).is_err());
    }
    #[tokio::test]
    async fn callback_rejects_forgery_and_does_not_consume_registration() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = oneshot::channel();
        let state = Arc::new(RegistrationState {
            config: Mutex::new(json!({})),
            root: dir.path().to_owned(),
            state: "a".repeat(64),
            secret_path: "/setup/secret".into(),
            action: "https://github.com/".into(),
            manifest: json!({"name":"<script>"}),
            busy: AtomicBool::new(false),
            completed: Mutex::new(Some(tx)),
            client: reqwest::Client::new(),
            conversion_base: url::Url::parse("https://api.github.com/app-manifests/").unwrap(),
        });
        let app = Router::new()
            .fallback(registration_handler)
            .with_state(state.clone());
        for path in [
            "/setup/callback?state=forged&code=123",
            "/setup/callback?state=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "/setup/callback?code=123",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(!state.busy.load(Ordering::Acquire));
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/setup")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/setup/secret")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 10000)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<script>"));
    }
    #[test]
    fn service_arguments_escape_systemd_expansion() {
        assert_eq!(
            systemd_quote("/home/a%/a\"b").unwrap(),
            "\"/home/a%%/a\\\"b\""
        );
        assert!(systemd_quote("a\nExecStart=bad").is_err());
        assert_eq!(
            systemd_quote("/home/$user/crow").unwrap(),
            "\"/home/$$user/crow\""
        );
    }

    #[test]
    fn model_selection_checks_both_model_and_reasoning_capability() {
        let models = vec![
            json!({"id":"model-id","model":"review-model","supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]}),
        ];
        assert!(
            crate::provider::validate_model_selection(
                &models,
                &json!({"model":"model-id","effort":"high"}),
                "Review",
            )
            .is_err()
        );
        crate::provider::validate_model_selection(
            &models,
            &json!({"model":"review-model","effort":"medium"}),
            "Subagent",
        )
        .unwrap();
        assert!(
            crate::provider::validate_model_selection(
                &models,
                &json!({"model":"unknown","effort":"medium"}),
                "Review"
            )
            .is_err()
        );
        assert!(
            crate::provider::validate_model_selection(
                &models,
                &json!({"model":"review-model","effort":"max"}),
                "Review"
            )
            .is_err()
        );
        assert!(
            crate::provider::validate_model_selection(
                &models,
                &json!({"model":null,"effort":null}),
                "Review"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn callback_persists_credentials_and_rejects_replays() {
        use tower::ServiceExt;
        let github = Router::new().route("/app-manifests/{code}/conversions",axum::routing::post(|| async {
            axum::Json(json!({"id":42,"pem":"private-pem","webhook_secret":"webhook-secret","slug":"crow-example"}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, github).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(RegistrationState {
            config: Mutex::new(config::defaults(directory.path())),
            root: directory.path().to_owned(),
            state: "a".repeat(64),
            secret_path: "/setup/secret".into(),
            action: "https://github.com/".into(),
            manifest: json!({}),
            busy: AtomicBool::new(false),
            completed: Mutex::new(Some(tx)),
            client: reqwest::Client::new(),
            conversion_base: url::Url::parse(&format!("http://{address}/app-manifests/")).unwrap(),
        });
        let app = Router::new()
            .fallback(registration_handler)
            .with_state(state);
        let path = format!(
            "/setup/callback?state={}&code=one-time-code",
            "a".repeat(64)
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        rx.await.unwrap().unwrap();
        let saved = config::load(directory.path()).unwrap();
        assert_eq!(saved["app"]["id"], 42);
        assert_eq!(saved["app"]["pem"], "private-pem");
        assert_eq!(saved["app"]["webhookSecret"], "webhook-secret");
        let replay = app
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        server.abort();
    }
}
