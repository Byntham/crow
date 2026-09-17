//! Human-readable command output. Machine output remains the original JSON in the CLI.
use serde_json::Value;
use std::fmt::Write;

const ACTIVE_LIMIT: usize = 10;
const RECENT_LIMIT: usize = 5;

/// Render a command result without exposing its internal record structure.
pub fn render(command: &str, value: &Value) -> String {
    let mut out = match command {
        "status" => status(value),
        "doctor" => doctor(value),
        "models" => models(value),
        "config" => configuration(value),
        "enroll" | "policy" | "repo-config" | "config-repo" => repository(command, value),
        "review" | "resume" | "restart" | "pause" => review(command, value),
        "catch-up" => catch_up(value),
        "release" => {
            let count = value["released"].as_u64().unwrap_or(0);
            if count == 0 { "No held reviews to release.".into() }
            else { format!("Released {} into the review queue.", counted(count as usize, "review")) }
        }
        "drain" => "New reviews are on hold. Active reviews will finish.\nRun crow undrain to accept new reviews again.".into(),
        "undrain" => "Crow is accepting new reviews again.".into(),
        "pair" => pairing(value),
        "cleanup" => cleanup(value),
        "backup" => format!("Encrypted backup saved.\n\n  File   {}\n  Saved  {}", scalar(&value["file"]), counted(list(&value["files"]).len(), "file")),
        "restore" => format!("Backup restored.\n\n  Folder    {}\n  Restored  {}\n\nOn review workers, run crow login to reconnect your account.\nRun crow start when you are ready.\nSaved provider sessions are not included in backups.", scalar(&value["root"]), counted(list(&value["files"]).len(), "file")),
        "update" => if value["updated"] == true { format!("Crow updated to {} and restarted.", scalar(&value["version"])) } else { "Crow is up to date.".into() },
        "start" => "Crow started. Run crow status to see repositories and reviews.".into(),
        "stop" => "Crow stopped. Run crow start when you are ready.".into(),
        "service-restart" => "Crow restarted with the saved configuration.".into(),
        "config-set" => {
            let key = value["key"].as_str().unwrap_or("");
            if key.is_empty() { "Configuration saved.\nRun crow service-restart to apply it.".into() }
            else { format!("Saved {}.\nRun crow service-restart to apply it.", clean(key)) }
        }
        _ => {
            let mut text = String::new();
            details(&mut text, value, 0);
            if text.is_empty() { "Done.".into() } else { text }
        }
    };
    if command == "status" {
        let updates = &value["updates"];
        if updates["available"] == true {
            out.push_str("\n\nAn update is available. Run crow update when ready.");
        }
        if let Some(warning) = updates["warning"].as_str() {
            let _ = write!(out, "\n\nUpdate check: {}", clean(warning));
        }
    }
    out.trim_end().to_owned()
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "Not set".into(),
        Value::String(s) => clean(s),
        Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => {
            if items.is_empty() {
                "None".into()
            } else {
                items.iter().map(scalar).collect::<Vec<_>>().join(", ")
            }
        }
        Value::Object(_) => "Configured".into(),
    }
}
fn counted(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
fn clipped(s: &str, limit: usize) -> String {
    let text = clean(s);
    if text.chars().count() > limit {
        format!(
            "{}...",
            text.chars()
                .take(limit.saturating_sub(3))
                .collect::<String>()
        )
    } else {
        text
    }
}
fn field(out: &mut String, label: &str, value: impl AsRef<str>) {
    let _ = writeln!(out, "  {label:<19} {}", clean(value.as_ref()));
}
fn section(out: &mut String, title: &str) {
    let _ = writeln!(out, "\n{title}");
}
fn table(out: &mut String, headers: &[&str], rows: &[Vec<String>], limits: &[usize]) {
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .enumerate()
                .map(|(i, s)| clipped(s, limits[i]))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(h.len())
        })
        .collect();
    for row in
        std::iter::once(headers.iter().map(|s| s.to_string()).collect::<Vec<_>>()).chain(rows)
    {
        out.push_str("  ");
        for (i, item) in row.iter().enumerate() {
            out.push_str(item);
            if i + 1 != row.len() {
                out.push_str(&" ".repeat(widths[i].saturating_sub(item.chars().count()) + 2));
            }
        }
        out.push('\n');
    }
}
fn state(v: &Value) -> String {
    match v.as_str().unwrap_or("") {
        "reviewing" => "Reviewing",
        "queued" => "Queued",
        "held" => "Held for approval",
        "retrying" => "Waiting to retry",
        "paused" => "Paused",
        "publishing" => "Posting review",
        "completed" => "Completed",
        "cancelled" => "Cancelled",
        "superseded" => "Replaced by newer code",
        "failed" => "Failed",
        "draining" => "Finishing active reviews",
        "running" => "Running",
        "stopped" => "Stopped",
        "stopping" => "Stopping",
        "connected" => "Connected",
        "connecting" => "Connecting",
        "disconnected" => "Disconnected",
        _ => return scalar(v),
    }
    .into()
}
fn timestamp(v: &Value) -> String {
    if let Some(ms) = v.as_i64().filter(|n| *n > 0) {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "Unknown".into())
    } else if let Some(s) = v.as_str() {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| {
                d.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|_| clean(s))
    } else {
        "Never".into()
    }
}
fn pr(job: &Value) -> String {
    if let (Some(repo), Some(number)) = (job["repo"].as_str(), job["number"].as_u64()) {
        format!("{} #{number}", clean(repo))
    } else {
        scalar(&job["key"])
    }
}
fn pr_summary(job: &Value, limit: usize) -> String {
    if let (Some(repo), Some(number)) = (job["repo"].as_str(), job["number"].as_u64()) {
        let suffix = format!(" #{number}");
        format!(
            "{}{}",
            clipped(repo, limit.saturating_sub(suffix.len())),
            suffix
        )
    } else {
        clipped(&pr(job), limit)
    }
}

fn full_names(out: &mut String, values: &[Value], key: &str, limit: usize, label: &str) {
    for value in values {
        if let Some(name) = value[key]
            .as_str()
            .filter(|name| clean(name).chars().count() > limit)
        {
            let _ = writeln!(out, "\n  {label}: {}", clean(name));
        }
    }
}

fn active(job: &Value) -> bool {
    !matches!(
        job["state"].as_str(),
        Some("completed" | "cancelled" | "superseded")
    )
}
fn job_rows(out: &mut String, jobs: &[&Value], limit: usize) {
    let rows = jobs
        .iter()
        .take(limit)
        .map(|j| {
            vec![
                pr_summary(j, 40),
                state(&j["state"]),
                timestamp(&j["updatedAt"]),
            ]
        })
        .collect::<Vec<_>>();
    table(
        out,
        &["Pull request", "Status", "Last updated"],
        &rows,
        &[40, 24, 20],
    );
    for job in jobs.iter().take(limit) {
        let reason = job["reason"].as_str().filter(|reason| !reason.is_empty());
        if pr(job).chars().count() > 40 {
            let _ = writeln!(out, "\n  Pull request: {}", pr(job));
            if let Some(reason) = reason {
                let _ = writeln!(out, "  {}", clipped(reason, 160));
            }
        } else if let Some(reason) = reason {
            let _ = writeln!(out, "\n  {}: {}", pr(job), clipped(reason, 160));
        }
    }
    if jobs.len() > limit {
        let _ = writeln!(
            out,
            "\n  {} more. Use crow status --format json for the full list.",
            jobs.len() - limit
        );
    }
}
fn status(v: &Value) -> String {
    if v.get("jobs").is_none() && v.get("repos").is_none() {
        let mut out = "Crow worker\n".to_owned();
        if let Some(message) = v["message"].as_str() {
            let _ = writeln!(out, "\n{}", clean(message));
            return out;
        }
        field(&mut out, "Status", state(&v["state"]));
        field(&mut out, "Service connection", state(&v["connection"]));
        field(&mut out, "Last reported", timestamp(&v["updatedAt"]));
        let jobs: Vec<_> = list(&v["active"]).iter().collect();
        section(&mut out, &format!("Active reviews ({})", jobs.len()));
        if jobs.is_empty() {
            out.push_str("  No reviews running.\n");
        } else {
            let rows = jobs
                .iter()
                .take(ACTIVE_LIMIT)
                .map(|j| vec![pr_summary(j, 60), state(&j["state"])])
                .collect::<Vec<_>>();
            table(&mut out, &["Pull request", "Status"], &rows, &[60, 24]);
            for job in jobs
                .iter()
                .take(ACTIVE_LIMIT)
                .filter(|job| pr(job).chars().count() > 60)
            {
                let _ = writeln!(out, "\n  Pull request: {}", pr(job));
            }
            if jobs.len() > ACTIVE_LIMIT {
                let _ = writeln!(
                    out,
                    "  {} more. Use crow status --format json for the full list.",
                    jobs.len() - ACTIVE_LIMIT
                );
            }
        }
        out.push_str("\nThis is the worker's last report. Run crow doctor to check its health.\n");
        return out;
    }
    let repos = list(&v["repos"]);
    let workers = list(&v["workers"]);
    let jobs = list(&v["jobs"]);
    let mut out = "Crow status\n".to_owned();
    field(
        &mut out,
        "Service",
        if v["draining"] == true {
            "Draining. New reviews are on hold."
        } else {
            "Accepting reviews"
        },
    );
    field(
        &mut out,
        "Overview",
        format!(
            "{} / {} / {}",
            counted(repos.len(), "repository").replace("repositorys", "repositories"),
            counted(workers.len(), "worker"),
            counted(jobs.len(), "review")
        ),
    );
    section(&mut out, "Repositories");
    if repos.is_empty() {
        out.push_str("  No repositories enrolled. Run crow enroll OWNER/REPO to add one.\n");
    } else {
        let rows = repos
            .iter()
            .map(|r| {
                vec![
                    scalar(&r["name"]),
                    if r["policy"] == "everyone" {
                        "Everyone".into()
                    } else {
                        scalar(&r["authors"])
                    },
                ]
            })
            .collect::<Vec<_>>();
        table(&mut out, &["Repository", "Reviews for"], &rows, &[46, 42]);
        full_names(&mut out, repos, "name", 46, "Full repository name");
    }
    section(&mut out, "Workers");
    if workers.is_empty() {
        out.push_str("  No workers paired. Run crow pair to connect a worker.\n");
    } else {
        let rows = workers
            .iter()
            .map(|w| {
                vec![
                    scalar(&w["id"]),
                    timestamp(&w["lastSeen"]),
                    if w["lastSeen"].as_i64().unwrap_or(0) == 0 {
                        "Awaiting connection".into()
                    } else {
                        scalar(&w["defaults"]["model"]).replace("Not set", "Provider default")
                    },
                ]
            })
            .collect::<Vec<_>>();
        table(
            &mut out,
            &["Worker", "Last connected", "Model"],
            &rows,
            &[36, 20, 30],
        );
        full_names(&mut out, workers, "id", 36, "Full worker ID");
    }
    let mut current: Vec<_> = jobs.iter().filter(|j| active(j)).collect();
    current.sort_by_key(|j| {
        (
            match j["state"].as_str() {
                Some("failed" | "paused" | "held") => 0,
                Some("reviewing" | "publishing") => 1,
                _ => 2,
            },
            std::cmp::Reverse(j["updatedAt"].as_i64().unwrap_or(0)),
        )
    });
    section(&mut out, &format!("Open reviews ({})", current.len()));
    if current.is_empty() {
        out.push_str("  No reviews waiting or running.\n");
    } else {
        job_rows(&mut out, &current, ACTIVE_LIMIT);
    }
    let mut recent: Vec<_> = jobs.iter().filter(|j| !active(j)).collect();
    recent.sort_by_key(|j| std::cmp::Reverse(j["updatedAt"].as_i64().unwrap_or(0)));
    if !recent.is_empty() {
        section(&mut out, "Recent reviews");
        job_rows(&mut out, &recent, RECENT_LIMIT);
    }
    if current.iter().any(|j| j["state"] == "held") {
        out.push_str("\nRun crow release to queue held reviews.\n");
    }
    if current.iter().any(|j| j["state"] == "paused") {
        out.push_str("\nResume a saved review: crow resume OWNER/REPO NUMBER\nStart it over: crow restart OWNER/REPO NUMBER\n");
    }
    if v["draining"] == true {
        out.push_str("\nRun crow undrain to accept new reviews again.\n");
    }
    out
}

fn doctor(v: &Value) -> String {
    let checks = list(&v["checks"]);
    let failed = checks.iter().filter(|c| c["ok"] != true).count();
    let mut out = if v["ok"] == true {
        format!("Crow health check: all {} checks passed.\n", checks.len())
    } else {
        format!(
            "Crow health check: {} need attention.\n",
            counted(failed, "check")
        )
    };
    for check in checks {
        diagnostic(&mut out, check, 0);
    }
    warnings(&mut out, v);
    out
}
fn diagnostic(out: &mut String, check: &Value, depth: usize) {
    let _ = writeln!(
        out,
        "\n{}[{}] {}",
        "  ".repeat(depth),
        if check["ok"] == true { "OK" } else { "FAIL" },
        clean(check["name"].as_str().unwrap_or("Check"))
    );
    let detail = &check["detail"];
    // Older services encode failed runtime diagnostics as a JSON string.
    let parsed = detail
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let detail = parsed.as_ref().unwrap_or(detail);
    if detail["checks"].is_array() {
        for child in list(&detail["checks"]) {
            diagnostic(out, child, depth + 1);
        }
        warnings(out, detail);
    } else if detail.get("repositories").is_some() {
        let _ = writeln!(
            out,
            "  {} repositories, {} workers, {} open reviews",
            scalar(&detail["repositories"]),
            scalar(&detail["workers"]),
            scalar(&detail["activeJobs"])
        );
        if detail["draining"] == true {
            out.push_str("  New reviews are on hold. Run crow undrain to resume.\n");
        }
    } else if detail["type"].is_string() {
        let _ = writeln!(
            out,
            "  {}{}",
            if detail["type"] == "chatgpt" {
                "ChatGPT subscription"
            } else {
                "Account proxy"
            },
            detail["email"]
                .as_str()
                .map(|s| format!(", {}", clean(s)))
                .unwrap_or_default()
        );
    } else if detail["service"] == "crow" {
        out.push_str("  Crow is reachable.\n");
    } else {
        details(out, detail, depth + 1);
    }
}
fn warnings(out: &mut String, v: &Value) {
    if let Some(w) = v["warning"].as_str() {
        let _ = writeln!(out, "\nWarning: {}", clean(w));
    }
    for w in list(&v["warnings"]) {
        let _ = writeln!(out, "\nWarning: {}", scalar(w));
    }
}
fn models(v: &Value) -> String {
    let mut out = "Available review models\n".to_owned();
    if v["cached"] == true {
        field(
            &mut out,
            "Catalog",
            "Saved copy. The provider could not be reached.",
        );
    }
    if !v["retrievedAt"].is_null() {
        field(&mut out, "Retrieved", timestamp(&v["retrievedAt"]));
    }
    out.push('\n');
    let rows = list(&v["models"])
        .iter()
        .map(|m| {
            vec![
                scalar(m.get("model").unwrap_or(&m["id"])),
                list(&m["supportedReasoningEfforts"])
                    .iter()
                    .map(|e| scalar(&e["reasoningEffort"]))
                    .collect::<Vec<_>>()
                    .join(", "),
                scalar(&m["defaultReasoningEffort"]),
            ]
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        out.push_str("No models available. Run crow login, then crow models again.\n");
    } else {
        table(
            &mut out,
            &["Model", "Reasoning effort", "Default effort"],
            &rows,
            &[usize::MAX, usize::MAX, usize::MAX],
        );
        out.push_str("\nChoose a model: crow config worker.model MODEL\nChoose effort:  crow config worker.effort LEVEL\n");
    }
    warnings(&mut out, v);
    out
}
fn duration(v: &Value) -> String {
    if v.is_null() {
        return "Inherited".into();
    }
    let ms = v.as_u64().unwrap_or(0);
    if ms == 0 {
        "No limit".into()
    } else if ms.is_multiple_of(3_600_000) {
        counted((ms / 3_600_000) as usize, "hour")
    } else if ms.is_multiple_of(60_000) {
        counted((ms / 60_000) as usize, "minute")
    } else if ms.is_multiple_of(1000) {
        counted((ms / 1000) as usize, "second")
    } else {
        format!("{ms} ms")
    }
}
fn configuration(v: &Value) -> String {
    let mut out = "Crow configuration\n".to_owned();
    field(
        &mut out,
        "Role",
        match v["role"].as_str() {
            Some("both") => "Service and review worker",
            Some("service") => "Connection service",
            Some("worker") => "Review worker",
            _ => "Not set",
        },
    );
    field(&mut out, "GitHub operator", scalar(&v["operator"]));
    if v["role"] != "worker" {
        section(&mut out, "Connection service");
        field(&mut out, "Public URL", scalar(&v["publicUrl"]));
        field(
            &mut out,
            "Local address",
            format!("{}:{}", scalar(&v["bind"]), scalar(&v["port"])),
        );
        field(
            &mut out,
            "GitHub App",
            v["app"]["slug"]
                .as_str()
                .unwrap_or(if v["app"].is_object() {
                    "Registered"
                } else {
                    "Not registered"
                }),
        );
        field(&mut out, "Public access", scalar(&v["ingress"]["type"]));
    }
    if v["role"] != "service" {
        section(&mut out, "Review worker");
        field(&mut out, "Worker ID", scalar(&v["worker"]["id"]));
        field(&mut out, "Service URL", scalar(&v["serviceUrl"]));
        settings(&mut out, &v["worker"]);
    }
    if v["role"] != "worker" {
        section(&mut out, "Scheduling and history");
        field(
            &mut out,
            "Catch-up scans",
            if v["catchUp"]["enabled"] == true {
                "Enabled"
            } else {
                "Disabled"
            },
        );
        field(
            &mut out,
            "Approval threshold",
            format!(
                "{} reviews in a catch-up batch",
                scalar(&v["catchUp"]["threshold"])
            ),
        );
        field(
            &mut out,
            "GitHub audit",
            format!("Every {}", duration(&v["auditIntervalMs"])),
        );
    }
    field(
        &mut out,
        "History retention",
        format!("{} days", scalar(&v["retentionDays"])),
    );
    out.push_str("\nCredentials are hidden.\nChange a setting: crow config KEY VALUE\nExample: crow config worker.concurrency 3\nUse crow config --help to see editable settings.\n");
    out
}
fn settings(out: &mut String, v: &Value) {
    for (key, label) in [("model", "Model"), ("effort", "Reasoning effort")] {
        if let Some(value) = v.get(key) {
            field(
                out,
                label,
                if value.is_null() {
                    "Provider default".into()
                } else {
                    scalar(value)
                },
            );
        }
    }
    if v.get("concurrency").is_some() {
        field(out, "Concurrent reviews", scalar(&v["concurrency"]));
    }
    if let Some(sub) = v.get("subagents") {
        let description = if sub["max"] == 0 {
            "Disabled".to_owned()
        } else {
            let limit = sub
                .get("max")
                .map(|value| format!("Up to {}", scalar(value)))
                .unwrap_or_else(|| "Inherited helper limit".into());
            let selection = if sub["mode"] == "inherit" {
                "same model and effort as the review".to_owned()
            } else if sub["mode"] == "configured" {
                format!(
                    "{}, {}",
                    sub.get("model")
                        .map(scalar)
                        .unwrap_or_else(|| "inherited model".into()),
                    sub.get("effort")
                        .map(|value| format!("{} effort", scalar(value)))
                        .unwrap_or_else(|| "inherited effort".into())
                )
            } else {
                let mut selection = "inherited helper mode".to_owned();
                if let Some(model) = sub.get("model") {
                    let _ = write!(selection, "; model {}", scalar(model));
                }
                if let Some(effort) = sub.get("effort") {
                    let _ = write!(selection, "; {} effort", scalar(effort));
                }
                selection
            };
            format!("{limit}; {selection}")
        };
        field(out, "Review helpers", description);
    }
    if let Some(retry) = v.get("retry") {
        let count = retry
            .get("count")
            .map(|value| format!("Up to {} retries", scalar(value)))
            .unwrap_or_else(|| "Inherited retry limit".into());
        let schedule = if retry["mode"] == "progressive" {
            "delays increase from 5 seconds to 5 minutes".to_owned()
        } else if retry["mode"].is_null() {
            if retry.get("delayMs").is_some() {
                format!(
                    "{} between attempts if using a fixed schedule",
                    duration(&retry["delayMs"])
                )
            } else {
                "inherited retry schedule".to_owned()
            }
        } else {
            format!("{} between attempts", duration(&retry["delayMs"]))
        };
        field(out, "Retries", format!("{count}; {schedule}"));
    }
    if let Some(timeout) = v.get("timeoutMs") {
        field(out, "Review time limit", duration(timeout));
    }
}
fn repository(command: &str, v: &Value) -> String {
    let mut out = format!(
        "{} {}.\n",
        if command == "enroll" {
            "Enrolled"
        } else {
            "Updated"
        },
        scalar(&v["name"])
    );
    field(
        &mut out,
        "Reviews for",
        if v["policy"] == "everyone" {
            "Everyone".into()
        } else {
            scalar(&v["authors"])
        },
    );
    field(&mut out, "Can request reviews", scalar(&v["requesters"]));
    field(&mut out, "Worker", scalar(&v["worker"]));
    if v["settings"].as_object().is_some_and(|s| !s.is_empty()) {
        section(&mut out, "Repository settings");
        settings(&mut out, &v["settings"]);
    }
    if command == "enroll" && !list(&v["excluded"]).is_empty() {
        let _ = write!(
            out,
            "\n{} already-open pull requests are excluded.\nTo include them: crow catch-up {} --include-backlog\n",
            list(&v["excluded"]).len(),
            scalar(&v["name"])
        );
    }
    out
}
fn review(command: &str, v: &Value) -> String {
    if let Some(reason) = v["skipped"].as_str() {
        return format!(
            "Review not queued: {}.",
            clean(reason).trim_end_matches('.')
        );
    }
    let target = if v["repo"].is_string() {
        format!(" for {}", pr(v))
    } else {
        String::new()
    };
    if command == "pause" {
        return format!(
            "Pause requested{target}.\nUse crow resume OWNER/REPO NUMBER to continue saved work."
        );
    }
    let mut out = format!("Review{target}: {}.\n", state(&v["state"]));
    if let Some(reason) = v["reason"].as_str() {
        let _ = writeln!(out, "\n{}", clean(reason));
    }
    if v["settings"].is_object() {
        settings(&mut out, &v["settings"]);
    }
    if let Some(url) = v["reviewUrl"].as_str() {
        let _ = writeln!(out, "\nView review: {}", clean(url));
    } else {
        out.push_str("\nRun crow status to follow its progress.\n");
    }
    out
}
fn catch_up(v: &Value) -> String {
    let mut out = "Catch-up scan complete.\n\n".to_owned();
    let rows = v
        .as_object()
        .map(|repos| {
            repos
                .iter()
                .map(|(repo, result)| {
                    vec![
                        clean(repo),
                        scalar(&result["queued"]),
                        scalar(&result["held"]),
                    ]
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        out.push_str("No repositories enrolled. Run crow enroll OWNER/REPO first.\n");
    } else {
        table(
            &mut out,
            &["Repository", "Queued", "Held for approval"],
            &rows,
            &[52, 10, 18],
        );
        if let Some(repos) = v.as_object() {
            for name in repos.keys().filter(|name| clean(name).chars().count() > 52) {
                let _ = writeln!(out, "\n  Full repository name: {}", clean(name));
            }
        }
    }
    if v.as_object()
        .is_some_and(|repos| repos.values().any(|r| r["held"].as_u64().unwrap_or(0) > 0))
    {
        out.push_str("\nLarge batches wait for your approval. Run crow release to queue them.\n");
    }
    out
}
fn pairing(v: &Value) -> String {
    let mut out="Worker pairing created.\n\nOn the worker computer, run crow setup --role worker.\nEnter these values when prompted:\n\n".to_owned();
    field(&mut out, "Service URL", scalar(&v["serviceUrl"]));
    field(&mut out, "Worker ID", scalar(&v["id"]));
    field(&mut out, "Pairing token", scalar(&v["token"]));
    out.push_str("\nKeep the pairing token private. It gives this worker access to Crow.\n");
    out
}
fn cleanup(v: &Value) -> String {
    let mut out = String::new();
    if let Some(service) = v.get("service") {
        out.push_str(&cleanup(service));
    }
    if let Some(worker) = v.get("worker") {
        out.push_str(&cleanup(worker));
    }
    if v["cleaned"] == true {
        out.push_str("Service history cleanup complete.\n");
    }
    if v["removed"].is_array() {
        let n = list(&v["removed"]).len();
        if n == 0 {
            out.push_str("No expired worker files to remove.\n");
        } else {
            let _ = writeln!(out, "Removed expired files for {}.", counted(n, "review"));
        }
    }
    warnings(&mut out, v);
    out
}
fn label(key: &str) -> String {
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        if c == '_' || c == '-' {
            out.push(' ');
        } else if i > 0 && c.is_uppercase() {
            out.push(' ');
            out.push(c.to_ascii_lowercase());
        } else if i == 0 {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}
fn secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key == "pem"
        || key.contains("privatekey")
}
fn details(out: &mut String, v: &Value, depth: usize) {
    let indent = "  ".repeat(depth.min(5));
    match v {
        Value::Object(fields) => {
            for (key, value) in fields {
                if secret(key) {
                    let _ = writeln!(out, "{indent}{}: [hidden]", label(key));
                } else if value.is_object() {
                    let _ = writeln!(out, "{indent}{}", label(key));
                    details(out, value, depth + 1);
                } else {
                    let _ = writeln!(out, "{indent}{}: {}", label(key), scalar(value));
                }
            }
        }
        _ => {
            let _ = writeln!(out, "{indent}{}", scalar(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn status_bounds_history_and_keeps_attention_first() {
        let mut jobs=(0..100).map(|n|json!({"repo":"owner/repo","number":n,"state":"completed","updatedAt":n+1,"session":"private-session"})).collect::<Vec<_>>();
        jobs.push(json!({"repo":"owner/repo","number":101,"state":"paused","reason":"Sign in again with crow login","settings":{"secret":"hidden"}}));
        let output = render(
            "status",
            &json!({"repos":[],"workers":[],"jobs":jobs,"draining":false}),
        );
        assert!(output.contains("Open reviews (1)"));
        assert!(output.contains("owner/repo #101"));
        assert!(output.contains("Sign in again with crow login"));
        assert!(output.contains("95 more"));
        assert!(!output.contains("owner/repo #94"));
        assert!(!output.contains("private-session"));
        assert!(output.lines().count() < 35);
    }
    #[test]
    fn configuration_hides_secrets_and_explains_defaults() {
        let mut config = crate::config::defaults(std::path::Path::new("/tmp/crow"));
        config["worker"]["token"] = json!("do-not-print");
        config["app"] =
            json!({"pem":"private-pem","webhookSecret":"private-hook","slug":"crow-app"});
        let output = render("config", &config);
        for secret in [
            "do-not-print",
            "private-pem",
            "private-hook",
            config["adminToken"].as_str().unwrap(),
        ] {
            assert!(!output.contains(secret));
        }
        assert!(output.contains("Provider default"));
        assert!(output.contains("No limit"));
        assert!(output.contains("1 hour"));
    }
    #[test]
    fn diagnostics_decode_nested_failures() {
        let runtime = json!({"ok":false,"checks":[{"name":"Codex version","ok":false,"detail":"Upgrade Codex"}]});
        let output = render(
            "doctor",
            &json!({"ok":false,"checks":[{"name":"review runtime","ok":false,"detail":runtime.to_string()}]}),
        );
        assert!(output.contains("[FAIL] Codex version"));
        assert!(output.contains("Upgrade Codex"));
        assert!(!output.contains("{\""));
    }
    #[test]
    fn worker_snapshot_never_claims_live_health() {
        let output = render(
            "status",
            &json!({"state":"running","connection":"connected","active":[],"updatedAt":1}),
        );
        assert!(output.contains("last report"));
        assert!(output.contains("No reviews running"));
        assert!(!output.contains("pid"));
    }
    #[test]
    fn long_targets_keep_their_full_names_and_pr_numbers() {
        let repo = "owner/a-repository-name-that-is-too-long-to-fit-in-the-table";
        let worker = "worker-with-an-identifier-that-is-longer-than-thirty-six-characters";
        let output = render(
            "status",
            &json!({"repos":[{"name":repo}],"workers":[{"id":worker}],"jobs":[{"repo":repo,"number":123,"state":"queued"}]}),
        );
        assert!(output.contains(repo));
        assert!(output.contains(worker));
        assert!(
            output
                .lines()
                .any(|line| line.contains("...") && line.contains("#123"))
        );
    }
    #[test]
    fn model_identifiers_remain_copyable() {
        let model = "provider-review-model-with-a-name-longer-than-thirty-eight-characters";
        let output = render(
            "models",
            &json!({"models":[{"model":model,"defaultReasoningEffort":"high","supportedReasoningEfforts":[{"reasoningEffort":"high"}]}]}),
        );
        assert!(output.contains(model));
    }
    #[test]
    fn partial_helper_overrides_preserve_explicit_model_and_effort() {
        let mut out = String::new();
        settings(
            &mut out,
            &json!({"subagents":{"model":"chosen-model","effort":"high"}}),
        );
        assert!(out.contains("chosen-model"));
        assert!(out.contains("high effort"));
        assert!(out.contains("Inherited helper limit"));
    }
    #[test]
    fn progressive_retries_describe_the_actual_schedule() {
        let mut out = String::new();
        settings(
            &mut out,
            &json!({"retry":{"mode":"progressive","count":3,"delayMs":123000}}),
        );
        assert!(out.contains("5 seconds to 5 minutes"));
        assert!(!out.contains("123 seconds"));
    }
    #[test]
    fn preserves_pairing_credentials_only_for_pair_command() {
        let input =
            json!({"serviceUrl":"https://crow.example","id":"worker-one","token":"pairing-secret"});
        assert!(render("pair", &input).contains("pairing-secret"));
        assert!(!render("unknown", &input).contains("pairing-secret"));
    }
    #[test]
    fn sanitizes_terminal_controls_and_handles_empty_results() {
        let output = render("review", &json!({"skipped":"Bad\u{1b}[2J\ninput"}));
        assert!(!output.contains('\u{1b}'));
        assert!(render("catch-up", &json!({})).contains("No repositories enrolled"));
        assert!(render("release", &json!({"released":0})).contains("No held reviews"));
        assert!(
            render(
                "cleanup",
                &json!({"service":{"cleaned":true},"worker":{"removed":[],"warnings":[]}})
            )
            .contains("No expired worker files")
        );
    }
}
