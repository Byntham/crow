//! Resumable installation and onboarding. Each external side effect is preceded
//! by persisting enough information to resume without replacing another route.
use crate::github::GitHubApi;
use crate::{config, operations, util};
use anyhow::{Context, Result, bail};
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

struct PromptInput {
    fd: std::os::fd::RawFd,
    flags: libc::c_int,
}

impl PromptInput {
    fn new(fd: std::os::fd::RawFd) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, flags })
    }

    async fn line(&self) -> io::Result<String> {
        let mut bytes = Vec::new();
        loop {
            // Read only through the newline. Later prompts and child processes
            // must retain any answers already waiting on the same stdin.
            let mut byte = 0_u8;
            let count = unsafe { libc::read(self.fd, (&mut byte as *mut u8).cast(), 1) };
            match count {
                0 => break,
                1 => {
                    bytes.push(byte);
                    if byte == b'\n' {
                        break;
                    }
                    if bytes.len() % 1024 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                _ => match io::Error::last_os_error() {
                    error if error.kind() == io::ErrorKind::Interrupted => continue,
                    error if error.kind() == io::ErrorKind::WouldBlock => {
                        // Unlike tokio::io::stdin, this leaves no blocking task
                        // waiting for input when setup or its runtime shuts down.
                        // It also supports redirected regular files, unlike epoll.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    error => return Err(error),
                },
            }
        }
        String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

impl Drop for PromptInput {
    fn drop(&mut self) {
        unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.flags) };
    }
}

async fn ask(label: &str, fallback: &str) -> Result<String> {
    let input = PromptInput::new(libc::STDIN_FILENO)?;
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
        answer = input.line() => answer?,
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
        operations::run("sudo", &["apt-get", "install", "-y", command], true).await?;
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
    validate_model_selection(models, worker, "Review")?;
    if worker["subagents"]["mode"] == "configured" {
        validate_model_selection(models, &worker["subagents"], "Subagent")?;
    }
    Ok(())
}

fn validate_model_selection(models: &[Value], selected: &Value, label: &str) -> Result<()> {
    let model_name = string(selected, "model");
    let model = models
        .iter()
        .find(|m| !model_name.is_empty() && m["model"] == model_name)
        .with_context(|| {
            format!("{label} model is not available from the provider: {model_name}")
        })?;
    let effort = string(selected, "effort");
    if effort.is_empty()
        || !model["supportedReasoningEfforts"]
            .as_array()
            .is_some_and(|efforts| efforts.iter().any(|e| e["reasoningEffort"] == effort))
    {
        bail!("{label} reasoning level {effort} is unsupported for {model_name}");
    }
    Ok(())
}

pub async fn setup(root: &Path, options: &Value) -> Result<()> {
    util::private_dir(root)?;
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
    let local_bin = user_home()?.join(".local/bin");
    if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|path| path == local_bin)
    {
        println!(
            "\nTo use the crow command in this terminal, run:\n  export PATH=\"$HOME/.local/bin:$PATH\""
        );
    }
    config["role"] = json!(role);
    apply_setup_port(&mut config, options.get("port").filter(|v| !v.is_null()))?;
    config::save(root, &config)?;
    let mut identity = Value::Null;
    if role == "worker" {
        println!(
            "\nConnect this worker\nRun crow pair on your service machine, then enter its connection details."
        );
        let previous = string(&config, "serviceUrl");
        config["serviceUrl"] = json!(util::https_url(
            &ask(
                "Connection-service HTTPS URL",
                if previous.starts_with("https:") {
                    previous
                } else {
                    ""
                }
            )
            .await?
        )?);
        config["worker"]["id"] =
            json!(ask("Worker ID from crow pair", string(&config["worker"], "id")).await?);
        let token = ask("Worker token from crow pair", "").await?;
        if token.is_empty() {
            bail!("Worker pairing token required");
        }
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
            register_app(&mut config, root).await?;
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
    let initial = models.iter().find(|model| model["isDefault"] == true).or_else(|| models.iter().find(|model| !string(&config["worker"],"model").is_empty() && model["model"] == config["worker"]["model"])).context("The provider did not report a default model. Retry model discovery before completing initial setup.")?;
    let previous = string(&config["worker"], "model");
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
    let selected = choose(
        "Review model",
        if previous.is_empty() {
            initial["model"].as_str().unwrap_or(string(initial, "id"))
        } else {
            previous
        },
        &model_choices,
    )
    .await?;
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
        if previous.is_empty() {
            string(model, "defaultReasoningEffort")
        } else {
            previous
        },
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

#[cfg(test)]
mod tests {
    use super::*;

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
            validate_model_selection(
                &models,
                &json!({"model":"model-id","effort":"high"}),
                "Review",
            )
            .is_err()
        );
        validate_model_selection(
            &models,
            &json!({"model":"review-model","effort":"medium"}),
            "Subagent",
        )
        .unwrap();
        assert!(
            validate_model_selection(
                &models,
                &json!({"model":"unknown","effort":"medium"}),
                "Review"
            )
            .is_err()
        );
        assert!(
            validate_model_selection(
                &models,
                &json!({"model":"review-model","effort":"max"}),
                "Review"
            )
            .is_err()
        );
        assert!(
            validate_model_selection(&models, &json!({"model":null,"effort":null}), "Review")
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
