//! Durable GitHub event handling, worker leases, and advisory publication.
use crate::{
    config,
    github::{GitHub, GitHubApi, GitHubError},
    report,
    store::Store,
    util,
};
use anyhow::{Result, anyhow, bail, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Mutex as AsyncMutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const ACTIVE: &[&str] = &[
    "queued",
    "held",
    "reviewing",
    "retrying",
    "paused",
    "publishing",
];
fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn n(v: &Value, key: &str) -> i64 {
    v[key].as_i64().unwrap_or(0)
}
fn b(v: &Value, key: &str) -> bool {
    v[key].as_bool().unwrap_or(false)
}
fn array(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}
fn object(v: &Value) -> Result<()> {
    ensure!(v.is_object(), "Expected an object");
    Ok(())
}
fn string<'a>(v: &'a Value, label: &str) -> Result<&'a str> {
    v.as_str().ok_or_else(|| anyhow!("Invalid {label}"))
}
fn strings(v: &Value) -> Result<Vec<String>> {
    v.as_array()
        .ok_or_else(|| anyhow!("Expected an array"))?
        .iter()
        .map(|v| Ok(string(v, "string")?.to_owned()))
        .collect()
}
fn patch(mut value: Value, changes: &Value) -> Value {
    if let (Some(a), Some(c)) = (value.as_object_mut(), changes.as_object()) {
        a.extend(c.clone());
    }
    value
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn matches_login(values: &Value, name: &str) -> bool {
    values.as_array().is_some_and(|xs| {
        xs.iter()
            .any(|x| x.as_str().is_some_and(|x| x.eq_ignore_ascii_case(name)))
    })
}
fn requesters(repo: &Value) -> &Value {
    repo.get("requesters").unwrap_or(&repo["authors"])
}
fn eligible(repo: &Value, pr: &Value) -> bool {
    s(pr, "state") == "open"
        && !b(pr, "draft")
        && (s(repo, "policy") == "everyone"
            || matches_login(&repo["authors"], s(&pr["user"], "login")))
}
fn ineligible_reason(repo: &Value, pr: Option<&Value>) -> &'static str {
    match pr {
        None => "PR is closed",
        Some(pr) if s(pr, "state") != "open" => "PR is closed",
        Some(pr) if b(pr, "draft") => "PR is a draft",
        Some(pr)
            if s(repo, "policy") != "everyone"
                && !matches_login(&repo["authors"], s(&pr["user"], "login")) =>
        {
            "PR author is not authorized"
        }
        _ => "PR is not eligible",
    }
}
fn comparison_key(c: &Value) -> String {
    format!("{}:{}:{}", s(c, "head"), s(c, "target"), s(c, "base"))
}
fn review_settings(v: &Value) -> Value {
    let mut out = json!({});
    for k in ["model", "effort", "subagents", "retry", "timeoutMs"] {
        if let Some(x) = v.get(k) {
            out[k] = x.clone();
        }
    }
    out
}
fn error_status(e: &anyhow::Error) -> u16 {
    e.downcast_ref::<GitHubError>().map_or(400, |e| e.status)
}
fn retry_after(e: &anyhow::Error) -> i64 {
    e.downcast_ref::<GitHubError>()
        .map_or(0, |e| e.retry_after.min(i64::MAX as u64) as i64)
}
fn valid_session(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        s.len() == 36
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-')
    })
}
fn valid_sha(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        (40..=64).contains(&s.len())
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
fn valid_logins(v: &Value) -> bool {
    let re = regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]{0,38}(?:\[bot\])?$").unwrap();
    v.as_array().is_some_and(|xs| {
        xs.iter()
            .all(|x| x.as_str().is_some_and(|x| re.is_match(x)))
    })
}

struct Service {
    config: Value,
    store: Mutex<Store>,
    github: Arc<dyn GitHubApi>,
    scans: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    shutdown: CancellationToken,
}
impl Service {
    fn db<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let db = self
            .store
            .lock()
            .map_err(|_| anyhow!("Database lock poisoned"))?;
        f(&db)
    }
    fn get(&self, kind: &str, key: &str) -> Result<Option<Value>> {
        self.db(|db| db.get(kind, key))
    }
    fn all(&self, kind: &str) -> Result<Vec<Value>> {
        self.db(|db| db.all(kind))
    }
    fn update(&self, id: &str, value: Value, lease: Option<&str>) -> Result<Value> {
        self.db(|db| db.update_job(id, &value, lease))
    }
    fn repo(&self, name: &str) -> Result<Value> {
        self.get("repos", &util::repo_name(name)?)?
            .ok_or_else(|| anyhow!("Repository is not enrolled"))
    }
    fn latest(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.all("jobs")?.into_iter().rfind(|j| s(j, "key") == key))
    }
    fn owns(&self, id: &str, lease: &str, state: &str) -> Result<bool> {
        Ok(self
            .get("jobs", id)?
            .is_some_and(|j| s(&j, "state") == state && s(&j, "lease") == lease))
    }
    async fn refresh(&self, repo: &Value, number: i64) -> Result<(Value, String)> {
        let token = self.github.token(repo).await?;
        let pr = self.github.pr(repo, number, &token).await?;
        Ok((pr, token))
    }
    async fn enqueue(&self, repo: &Value, number: i64, opts: &Value) -> Result<Value> {
        ensure!(number > 0, "Invalid pull request number");
        let (pr, _) = self.refresh(repo, number).await?;
        let mut repo = self.repo(s(repo, "name"))?;
        let requester = s(opts, "requester");
        if !requester.is_empty() && !matches_login(requesters(&repo), requester) {
            return Ok(json!({"skipped":"Requester is no longer authorized"}));
        }
        if !eligible(&repo, &pr) {
            for j in self.all("jobs")?.iter().filter(|j| {
                s(j, "repo") == s(&repo, "name")
                    && n(j, "number") == number
                    && ACTIVE.contains(&s(j, "state"))
            }) {
                self.update(
                    s(j, "id"),
                    json!({"state":"cancelled","reason":ineligible_reason(&repo,Some(&pr))}),
                    None,
                )?;
            }
            return Ok(json!({"skipped":"PR is not eligible"}));
        }
        if b(opts, "event") || b(opts, "manual") {
            repo["excluded"] = json!(
                array(&repo["excluded"])
                    .into_iter()
                    .filter(|x| x.as_i64() != Some(number))
                    .collect::<Vec<_>>()
            );
            self.db(|db| db.enroll(&repo).map(|_| ()))?;
        } else if array(&repo["excluded"]).contains(&json!(number)) {
            return Ok(json!({"skipped":"Initial backlog excluded"}));
        }
        self.db(|db| db.queue(&repo, &pr, opts))
    }
    async fn catch_up(&self, name: &str, include_backlog: bool) -> Result<Value> {
        let lock = {
            self.scans
                .lock()
                .map_err(|_| anyhow!("Scan lock poisoned"))?
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let initial = self.repo(name)?;
        let token = self.github.token(&initial).await?;
        let prs = self.github.prs(&initial, &token).await?;
        let mut repo = self.repo(name)?;
        if include_backlog {
            repo["excluded"] = json!([]);
            self.db(|db| db.enroll(&repo).map(|_| ()))?;
        }
        let jobs = self.all("jobs")?;
        for j in jobs
            .iter()
            .filter(|j| s(j, "repo") == name && ACTIVE.contains(&s(j, "state")))
        {
            let current = prs.iter().find(|p| n(p, "number") == n(j, "number"));
            if current.is_none_or(|pr| !eligible(&repo, pr)) {
                self.update(s(j,"id"),json!({"state":"cancelled","autoRecover":false,"reason":ineligible_reason(&repo,current)}),None)?;
            }
        }
        let fresh: Vec<_> = prs
            .iter()
            .filter(|pr| {
                eligible(&repo, pr)
                    && !array(&repo["excluded"]).contains(&pr["number"])
                    && !jobs.iter().any(|j| {
                        s(j, "repo") == name
                            && j["number"] == pr["number"]
                            && j["head"] == pr["head"]["sha"]
                            && j["target"] == pr["base"]["ref"]
                            && (ACTIVE.contains(&s(j, "state"))
                                || (s(j, "state") == "completed"
                                    && j["comparison"]["targetSha"] == pr["base"]["sha"]))
                    })
            })
            .collect();
        let held = fresh.len() as i64 > n(&self.config["catchUp"], "threshold");
        for pr in &fresh {
            self.db(|db| db.queue(&repo, pr, &json!({"held":held})))?;
        }
        Ok(json!({"queued":if held {0}else{fresh.len()},"held":if held {fresh.len()}else{0}}))
    }
    async fn handle_event(&self, e: &Value) -> Result<()> {
        if s(e, "repo").is_empty() {
            return Ok(());
        }
        let name = util::repo_name(s(e, "repo"))?;
        let Some(repo) = self.get("repos", &name)? else {
            return Ok(());
        };
        if n(e, "occurredAt") > 0 && n(e, "occurredAt") < n(&repo, "enrolledAt") {
            return Ok(());
        }
        match s(e, "type") {
            "pull_request"
                if [
                    "opened",
                    "reopened",
                    "synchronize",
                    "ready_for_review",
                    "edited",
                    "closed",
                    "converted_to_draft",
                ]
                .contains(&s(e, "action"))
                    && n(e, "number") > 0 =>
            {
                let trigger = match s(e, "action") {
                    "synchronize" => "Pull request updated".to_owned(),
                    "opened" => "Pull request opened".to_owned(),
                    x => format!("Pull request {x}"),
                };
                self.enqueue(
                    &repo,
                    n(e, "number"),
                    &json!({"event":true,"trigger":trigger}),
                )
                .await?;
            }
            "issue_comment"
                if b(e, "request")
                    && n(e, "number") > 0
                    && matches_login(requesters(&repo), s(e, "actor")) =>
            {
                if s(e, "command") == "pause" {
                    for j in self.all("jobs")?.iter().filter(|j| {
                        s(j, "repo") == name
                            && j["number"] == e["number"]
                            && ACTIVE.contains(&s(j, "state"))
                    }) {
                        self.update(s(j,"id"),json!({"state":"paused","autoRecover":false,"reason":"Paused by the operator","trigger":"Pause command"}),None)?;
                    }
                } else {
                    if s(e, "command") == "resume" {
                        let current = self.latest(&format!("{}#{}", name, n(e, "number")))?;
                        if current.as_ref().is_none_or(|j| {
                            !["paused", "held"].contains(&s(j, "state"))
                                || (s(j, "state") == "paused"
                                    && n(j, "startedAt") > 0
                                    && j["session"].is_null()
                                    && j["report"].is_null())
                        }) {
                            return Ok(());
                        }
                    }
                    let trigger = match s(e, "command") {
                        "resume" => "Resume command",
                        "restart" => "Restart command",
                        _ => "Manual request",
                    };
                    self.enqueue(&repo,n(e,"number"),&json!({"manual":true,"restart":s(e,"command")=="restart","requester":e["actor"],"trigger":trigger})).await?;
                }
            }
            "push" if s(e, "ref").starts_with("refs/heads/") => {
                let token = self.github.token(&repo).await?;
                let prs = self.github.prs(&repo, &token).await?;
                let current = self.repo(&name)?;
                for pr in prs.iter().filter(|pr| {
                    s(&pr["base"], "ref") == &s(e, "ref")[11..]
                        && eligible(&current, pr)
                        && !array(&current["excluded"]).contains(&pr["number"])
                }) {
                    self.db(|db| db.queue(&current, pr, &json!({})))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    async fn status(&self, j: &Value) -> Result<()> {
        let Some(bot_id) = self.config["app"]["botId"].as_i64().filter(|x| *x > 0) else {
            return Ok(());
        };
        let Some(repo) = self.get("repos", s(j, "repo"))? else {
            return Ok(());
        };
        if self.latest(s(j, "key"))?.is_none_or(|x| x["id"] != j["id"]) {
            return Ok(());
        }
        let state = s(j, "state");
        let icon = match state {
            "completed" => "✅",
            "paused" => "⏸️",
            "cancelled" => "❌",
            _ => "🔄",
        };
        let title = format!(
            "{}{}",
            state.get(..1).unwrap_or("").to_uppercase(),
            state.get(1..).unwrap_or("")
        );
        let head = s(j, "head");
        let trigger =
            j["trigger"]
                .as_str()
                .filter(|x| !x.is_empty())
                .unwrap_or(if b(j, "manual") {
                    "Manual request"
                } else {
                    "Pull request event"
                });
        let date = chrono::DateTime::from_timestamp_millis(n(j, "updatedAt"))
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut body = format!(
            "<!-- crow-status:v1 -->\n| Status | Commit | Review trigger |\n| --- | --- | --- |\n| {icon} {title} | [{}](https://github.com/{}/commit/{head}) | {trigger} |\n\nUpdated {date}.",
            head.get(..8).unwrap_or(head),
            s(j, "repo")
        );
        body.push_str(&format!(
            "\n\n**Runtime testing:** {}",
            crate::runtime_status::render(&j["runtime"], state)
        ));
        if !s(j, "reason").is_empty() {
            body.push_str(&format!("\n\n{}", s(j, "reason")));
        }
        if !s(j, "reviewUrl").is_empty() {
            body.push_str(&format!("\n\n[Read review]({})", s(j, "reviewUrl")));
        }
        if state == "paused" {
            body.push_str("\n\nComment `/crow resume` to continue saved work, or `/crow restart` to start again.");
        }
        let key = s(j, "key");
        let prev = self.get("status", key)?;
        if prev.as_ref().is_some_and(|p| {
            s(p, "body") == body
                || (p["state"] == j["state"]
                    && p["trigger"] == j["trigger"]
                    && p["head"] == j["head"]
                    && p["runtime"] == j["runtime"]
                    && now() - n(p, "updatedAt") < 60000)
        }) {
            return Ok(());
        }
        let token = self.github.token(&repo).await?;
        let result = self
            .github
            .status(
                &repo,
                n(j, "number"),
                &token,
                &body,
                bot_id,
                prev.as_ref().and_then(|p| p["id"].as_i64()),
            )
            .await?;
        self.db(|db|db.put("status",key,&json!({"id":result["id"],"body":body,"state":j["state"],"trigger":j["trigger"],"head":j["head"],"runtime":j["runtime"],"updatedAt":now()})))
    }
    async fn publish(&self, j: &Value) -> Result<()> {
        let (pr, token) = self
            .refresh(&self.repo(s(j, "repo"))?, n(j, "number"))
            .await?;
        let repo = self.repo(s(j, "repo"))?;
        if self
            .get("jobs", s(j, "id"))?
            .is_none_or(|j| s(&j, "state") != "publishing")
        {
            return Ok(());
        }
        if !eligible(&repo, &pr)
            || pr["head"]["sha"] != j["head"]
            || pr["base"]["ref"] != j["target"]
        {
            self.update(s(j, "id"), json!({"state":"superseded"}), None)?;
            if eligible(&repo, &pr) {
                self.db(|db| db.queue(&repo, &pr, &json!({})))?;
            }
            return Ok(());
        }
        if pr["base"]["sha"] != j["comparison"]["targetSha"] {
            self.update(s(j,"id"),json!({"state":"queued","nextAt":0,"reason":"Target branch changed; verifying the comparison before publication."}),None)?;
            return Ok(());
        }
        let reviews = self.github.reviews(&repo, n(j, "number"), &token).await?;
        let bot = self.config["app"]["botId"].as_i64();
        let owned = |r: &&Value| bot.is_some_and(|bot| r["user"]["id"].as_i64() == Some(bot));
        let mut published = reviews
            .iter()
            .filter(owned)
            .find(|r| report::metadata(s(r, "body")).is_some_and(|m| m["job"] == j["id"]))
            .cloned();
        let history = self
            .get("findings", s(j, "key"))?
            .map(|v| array(&v))
            .unwrap_or_default();
        if published.is_none() {
            if self
                .get("jobs", s(j, "id"))?
                .is_none_or(|j| s(&j, "state") != "publishing")
            {
                return Ok(());
            }
            let prior = reviews
                .iter()
                .filter(owned)
                .filter_map(|r| {
                    let m = report::metadata(s(r, "body"))?;
                    (m["job"] != j["id"]).then(|| json!({"url":r["html_url"],"head":m["head"]}))
                })
                .collect::<Vec<_>>();
            let body = report::report_body(j, &history, &prior)?;
            let comments = report::inline_comments(&j["report"], s(j, "patch"), &history);
            published = Some(
                self.github
                    .publish(
                        &repo,
                        n(j, "number"),
                        &token,
                        &body,
                        s(j, "head"),
                        &comments,
                    )
                    .await?,
            );
        }
        let url = published.unwrap()["html_url"].clone();
        let mut records = history;
        for f in array(&j["report"]["findings"]) {
            let new = json!({"id":f["id"],"title":f["title"],"url":url});
            if let Some(x) = records.iter_mut().find(|r| r["id"] == f["id"]) {
                *x = new;
            } else {
                records.push(new);
            }
        }
        self.db(|db| {
            db.tx(|db| {
                db.put("findings", s(j, "key"), &json!(records))?;
                db.put(
                    "completed",
                    &format!("{}:{}", s(j, "key"), comparison_key(&j["comparison"])),
                    &json!({"job":j["id"],"reviewUrl":url,"comparison":j["comparison"]}),
                )
            })
        })?;
        let current = self
            .get("jobs", s(j, "id"))?
            .ok_or_else(|| anyhow!("Publication record disappeared"))?;
        let state = s(&current, "state");
        let (next, reason) = match state {
            "paused" => ("paused", json!("Paused by the operator")),
            "superseded" => (
                "superseded",
                json!(
                    "A newer revision arrived during publication. This report covers the pinned earlier commit."
                ),
            ),
            "cancelled" => ("cancelled", current["reason"].clone()),
            _ => ("completed", Value::Null),
        };
        self.update(
            s(j, "id"),
            json!({"state":next,"reviewUrl":url,"reason":reason}),
            None,
        )?;
        Ok(())
    }
    async fn tick(&self) -> Result<()> {
        let mut deferred = HashSet::new();
        for e in self.db(|db| db.events())? {
            let repository = e["repo"].as_str().unwrap_or(s(&e, "id")).to_lowercase();
            if deferred.contains(&repository) {
                continue;
            }
            if n(&e, "nextAt") > now() {
                deferred.insert(repository);
                continue;
            }
            match self.handle_event(&e).await {
                Ok(()) => self.db(|db| db.event_done(s(&e, "id")))?,
                Err(err) => {
                    let delay = (5000_i64 * 2_i64.pow(n(&e, "retries").clamp(0, 6) as u32))
                        .min(300000)
                        .max(retry_after(&err).min(86400000));
                    self.db(|db| db.defer_event(&e, now() + delay))?;
                    deferred.insert(repository);
                    eprintln!("Event deferred: {err}");
                }
            }
        }
        for j in self.all("jobs")? {
            if s(&j, "state") == "reviewing" && now() - n(&j, "updatedAt") > 90000 {
                self.update(s(&j,"id"),json!({"state":"paused","autoRecover":true,"reason":"Worker disconnected. Saved work will resume when it reconnects."}),None)?;
            }
        }
        let mut latest = HashMap::new();
        for j in self.all("jobs")? {
            latest.insert(s(&j, "key").to_owned(), j);
        }
        for j in latest.values() {
            if let Err(e) = self.status(j).await {
                eprintln!("GitHub status deferred: {e}");
            }
        }
        Ok(())
    }
    async fn publish_tick(&self) -> Result<()> {
        for j in self
            .all("jobs")?
            .iter()
            .filter(|j| s(j, "state") == "publishing" && n(j, "publishAt") < now())
        {
            let result =
                if j["comparison"].is_null() || j["report"].is_null() || j["settings"].is_null() {
                    Err(anyhow!("Incomplete publication record"))
                } else {
                    self.publish(j).await
                };
            if let Err(e) = result {
                self.update(s(j,"id"),json!({"publishAt":now()+30000.max(retry_after(&e)),"reason":"GitHub publication failed; the completed report is saved and will be retried."}),None)?;
                eprintln!("Publication deferred: {e}");
            }
        }
        Ok(())
    }
    async fn audit(&self) -> Result<()> {
        if self.config["app"].is_null() {
            return Ok(());
        }
        self.db(|db| db.prune(n(&self.config, "retentionDays")))?;
        self.github.audit().await
    }
    async fn admin(&self, action: &str, a: &Value) -> Result<Value> {
        validate_admin(a)?;
        match action {
            "pair" => {
                let worker = json!({"id":a["id"].as_str().unwrap_or(&util::id()),"token":a["token"].as_str().unwrap_or(&format!("{}{}",util::id(),util::id()))});
                let id = s(&worker, "id");
                ensure!(
                    !id.is_empty()
                        && id.len() <= 100
                        && id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                        && s(&worker, "token").len() >= 32,
                    "Invalid worker ID or pairing token"
                );
                self.db(|db| {
                    db.tx(|db| {
                        ensure!(
                            db.get("workers", id)?.is_none(),
                            "Worker ID is already paired"
                        );
                        db.put("workers", id, &worker)
                    })
                })?;
                Ok(worker)
            }
            "enroll" => {
                let name = util::repo_name(string(&a["repo"], "repository")?)?;
                let existing = self.get("repos", &name)?;
                ensure!(
                    existing.is_none() || b(a, "reenroll"),
                    "Repository is already enrolled. Use crow repo-config to change it, or --reenroll after changing the GitHub App installation."
                );
                ensure!(
                    !s(&self.config, "operator").is_empty(),
                    "Complete operator setup first"
                );
                let token = a["githubToken"].as_str();
                let user = self.github.request("/user", token, "GET", None).await?;
                ensure!(
                    string(&user["login"], "GitHub login")?
                        .eq_ignore_ascii_case(s(&self.config, "operator")),
                    "GitHub login does not match the Crow operator"
                );
                let info = self
                    .github
                    .request(&format!("/repos/{name}"), token, "GET", None)
                    .await?;
                ensure!(
                    b(&info["permissions"], "admin") || b(&info["permissions"], "maintain"),
                    "Repository enrollment requires admin or maintain authority"
                );
                let installation = self.github.installation(&name).await?;
                if let Some(mut repo) = existing {
                    repo["installation"] = installation["id"].clone();
                    self.db(|db| db.enroll(&repo).map(|_| ()))?;
                    return Ok(repo);
                }
                let mut repo = json!({"name":name,"installation":installation["id"],"worker":a["worker"].as_str().unwrap_or(s(&self.config["worker"],"id")),"policy":a["policy"].as_str().unwrap_or("selected"),"authors":a.get("authors").cloned().unwrap_or_else(||json!([self.config["operator"]])),"requesters":[self.config["operator"]],"settings":a.get("settings").cloned().unwrap_or_else(||json!({})),"enrolledAt":now(),"excluded":[]});
                ensure!(
                    self.get("workers", s(&repo, "worker"))?.is_some(),
                    "Pair the worker before enrolling repositories"
                );
                ensure!(
                    valid_logins(&repo["authors"]) && !array(&repo["authors"]).is_empty(),
                    "Invalid author policy"
                );
                config::settings(&self.config, Some(&repo))?;
                let token = self.github.token(&repo).await?;
                let prs = self.github.prs(&repo, &token).await?;
                repo["excluded"] =
                    json!(prs.iter().map(|p| p["number"].clone()).collect::<Vec<_>>());
                self.db(|db| db.enroll(&repo).map(|_| ()))?;
                if b(a, "includeBacklog") {
                    self.catch_up(&name, true).await?;
                }
                Ok(repo)
            }
            "config-repo" => {
                let mut repo = self.repo(string(&a["repo"], "repository")?)?;
                for k in ["policy", "authors", "requesters", "worker", "settings"] {
                    if let Some(v) = a.get(k) {
                        repo[k] = v.clone();
                    }
                }
                ensure!(
                    ["selected", "everyone"].contains(&s(&repo, "policy"))
                        && valid_logins(&repo["authors"])
                        && valid_logins(&repo["requesters"])
                        && self.get("workers", s(&repo, "worker"))?.is_some(),
                    "Invalid repository policy or worker"
                );
                config::settings(&self.config, Some(&repo))?;
                self.db(|db| db.enroll(&repo).map(|_| ()))?;
                for job in self.all("jobs")?.iter().filter(|j| {
                    j["repo"] == repo["name"]
                        && !s(j, "author").is_empty()
                        && ACTIVE.contains(&s(j, "state"))
                        && s(&repo, "policy") != "everyone"
                        && !matches_login(&repo["authors"], s(j, "author"))
                }) {
                    self.update(s(job,"id"),json!({"state":"cancelled","autoRecover":false,"reason":"PR author is no longer authorized"}),None)?;
                }
                Ok(repo)
            }
            "review" | "resume" | "restart" => {
                let repo = self.repo(string(&a["repo"], "repository")?)?;
                let number = number_argument(a)?;
                let mut chosen_settings = None;
                if action == "resume" {
                    let current = self.latest(&format!("{}#{number}", s(&repo, "name")))?;
                    ensure!(
                        current.as_ref().is_some_and(|j| ["paused", "held"]
                            .contains(&s(j, "state"))
                            && !(s(j, "state") == "paused"
                                && n(j, "startedAt") > 0
                                && j["session"].is_null()
                                && j["report"].is_null())),
                        "No paused review with saved work to resume"
                    );
                    if a.get("model").is_some() || a.get("effort").is_some() {
                        let job = current.as_ref().unwrap();
                        let worker = self
                            .get("workers", s(&repo, "worker"))?
                            .unwrap_or(Value::Null);
                        let settings = if !job["settings"].is_null() {
                            job["settings"].clone()
                        } else {
                            let cfg = patch(
                                self.config.clone(),
                                &json!({"worker":patch(self.config["worker"].clone(),&worker["defaults"])}),
                            );
                            config::settings(&cfg, Some(&repo))?
                        };
                        let mut chosen = review_settings(&settings);
                        for k in ["model", "effort"] {
                            if let Some(v) = a.get(k) {
                                chosen[k] = v.clone();
                            }
                        }
                        config::settings(
                            &patch(
                                self.config.clone(),
                                &json!({"worker":patch(self.config["worker"].clone(),&chosen)}),
                            ),
                            None,
                        )?;
                        chosen_settings = Some(chosen);
                    }
                }
                let mut job = self
                    .enqueue(
                        &repo,
                        number,
                        &json!({"manual":true,"restart":action=="restart"}),
                    )
                    .await?;
                if let Some(chosen) = chosen_settings.filter(|_| job.get("id").is_some()) {
                    let mut changes = array(&job["settingsChanges"]);
                    changes.push(
                        json!({"model":chosen["model"],"effort":chosen["effort"],"at":now()}),
                    );
                    job = self.update(
                        s(&job, "id"),
                        json!({"settings":chosen,"settingsChanges":changes}),
                        None,
                    )?;
                }
                Ok(job)
            }
            "pause" => {
                let repo = self.repo(string(&a["repo"], "repository")?)?;
                let number = number_argument(a)?;
                for j in self.all("jobs")?.iter().filter(|j| {
                    j["repo"] == repo["name"]
                        && n(j, "number") == number
                        && ACTIVE.contains(&s(j, "state"))
                }) {
                    self.update(s(j,"id"),json!({"state":"paused","autoRecover":false,"reason":"Paused by the operator"}),None)?;
                }
                Ok(json!({"paused":true}))
            }
            "catch-up" => {
                let mut out = json!({});
                let repos = if let Some(name) = a["repo"].as_str() {
                    vec![self.repo(name)?]
                } else {
                    self.all("repos")?
                };
                for repo in repos {
                    out[s(&repo, "name")] = self
                        .catch_up(s(&repo, "name"), b(a, "includeBacklog"))
                        .await?;
                }
                Ok(out)
            }
            "release" => {
                let name = a["repo"].as_str().map(util::repo_name).transpose()?;
                let mut count = 0;
                for j in self.all("jobs")?.iter().filter(|j| {
                    s(j, "state") == "held" && name.as_ref().is_none_or(|name| s(j, "repo") == name)
                }) {
                    self.update(s(j, "id"), json!({"state":"queued"}), None)?;
                    count += 1;
                }
                Ok(json!({"released":count}))
            }
            "cleanup" => {
                self.db(|db| db.prune(n(&self.config, "retentionDays")))?;
                Ok(json!({"cleaned":true}))
            }
            "drain" => {
                self.db(|db| db.put("state", "drain", &json!(true)))?;
                Ok(json!({"draining":true}))
            }
            "undrain" => {
                self.db(|db| db.delete("state", "drain"))?;
                Ok(json!({"draining":false}))
            }
            _ => bail!("Unknown administrative action"),
        }
    }
    async fn worker_action(&self, action: &str, a: &Value, mut worker: Value) -> Result<Value> {
        object(a)?;
        worker["lastSeen"] = json!(now());
        if action == "next" && a.get("defaults").is_some() {
            object(&a["defaults"])?;
            let mut candidate = self.config["worker"].clone();
            for key in [
                "model",
                "effort",
                "subagents",
                "retry",
                "timeoutMs",
                "concurrency",
            ] {
                if let Some(v) = a["defaults"].get(key) {
                    candidate[key] = v.clone();
                }
            }
            let cfg = patch(self.config.clone(), &json!({"worker":candidate}));
            let defaults = config::settings(&cfg, None)?;
            worker["defaults"] = patch(
                review_settings(&defaults),
                &json!({"concurrency":defaults["concurrency"]}),
            );
        }
        self.db(|db| db.put("workers", s(&worker, "id"), &worker))?;
        if action == "next" {
            if let Some(sessions) = a["sessions"].as_object() {
                for (id, session) in sessions {
                    if let Some(job) = self.get("jobs", id)?
                        && job["worker"] == worker["id"]
                        && job["session"].is_null()
                        && b(&job, "autoRecover")
                        && s(&job, "state") == "paused"
                        && valid_session(session)
                    {
                        self.update(id, json!({"session":session}), None)?;
                    }
                }
            }
            let active = if a["active"].is_array() {
                strings(&a["active"])?
            } else {
                vec![]
            };
            if a["active"].is_array() {
                for job in self.all("jobs")?.iter().filter(|j| {
                    j["worker"] == worker["id"]
                        && b(j, "autoRecover")
                        && s(j, "state") == "paused"
                        && !active.iter().any(|id| id == s(j, "id"))
                }) {
                    let usable = !job["session"].is_null() || !job["report"].is_null();
                    self.update(s(job,"id"),json!({"state":if usable{"queued"}else{"paused"},"autoRecover":false,"reason":if usable{Value::Null}else{json!("Restart required: no saved provider session")}}),None)?;
                }
            }
            let job = self.db(|db| {
                if db
                    .get("state", "drain")?
                    .is_some_and(|v| v.as_bool() == Some(true))
                    || db
                        .get("state", "cooldown")?
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                        > now()
                {
                    return Ok(None);
                }
                let jobs = db.all("jobs")?;
                let concurrency = worker["defaults"]["concurrency"]
                    .as_i64()
                    .unwrap_or(n(&self.config["worker"], "concurrency"));
                if jobs
                    .iter()
                    .filter(|j| j["worker"] == worker["id"] && s(j, "state") == "reviewing")
                    .count() as i64
                    >= concurrency
                {
                    return Ok(None);
                }
                let keys = jobs
                    .iter()
                    .filter(|j| {
                        j["worker"] == worker["id"] && active.iter().any(|id| id == s(j, "id"))
                    })
                    .map(|j| s(j, "key").to_owned())
                    .collect::<Vec<_>>();
                db.claim(s(&worker, "id"), &active, &keys)
            })?;
            let Some(job) = job else {
                return Ok(Value::Null);
            };
            let dispatch=async {
                let assigned_repo = self.repo(s(&job,"repo"))?;
                let (pr,_) = self.refresh(&assigned_repo,n(&job,"number")).await?;
                // Publication credentials stay in the service. Workers only fetch source.
                let token = self.github.checkout_token(&assigned_repo).await?;
                if !self.owns(s(&job,"id"),s(&job,"lease"),"reviewing")? {return Ok(Value::Null);}
                let repo=self.repo(s(&job,"repo"))?;
                if !eligible(&repo,&pr) || pr["head"]["sha"]!=job["head"] || pr["base"]["ref"]!=job["target"] || repo["worker"]!=job["worker"] {
                    self.update(s(&job,"id"),json!({"state":"superseded"}),Some(s(&job,"lease")))?;
                    if eligible(&repo,&pr){self.db(|db|db.queue(&repo,&pr,&json!({})))?;}
                    return Ok(Value::Null);
                }
                let effective=if !job["settings"].is_null(){review_settings(&job["settings"])}else{let cfg=patch(self.config.clone(),&json!({"worker":patch(self.config["worker"].clone(),&worker["defaults"])}));review_settings(&config::settings(&cfg,Some(&repo))?)};
                self.update(s(&job,"id"),json!({"settings":effective,"author":pr["user"]["login"]}),Some(s(&job,"lease")))?;
                Ok(json!({"job":patch(job.clone(),&json!({"settings":effective})),"pr":pr,"token":token,"repo":repo}))
            }.await;
            if dispatch.is_err() && self.owns(s(&job, "id"), s(&job, "lease"), "reviewing")? {
                self.update(s(&job,"id"),json!({"state":"queued","nextAt":now()+30000,"reason":"GitHub connection failed; dispatch will retry."}),Some(s(&job,"lease")))?;
            }
            return dispatch;
        }
        if action == "ping" {
            return Ok(json!({"ok":true,"id":worker["id"]}));
        }
        if action == "maintenance" {
            let jobs=self.all("jobs")?.iter().filter(|j|j["worker"]==worker["id"]).map(|j|json!({"id":j["id"],"state":j["state"],"updatedAt":j["updatedAt"],"session":j["session"]})).collect::<Vec<_>>();
            return Ok(json!({"jobs":jobs,"retentionDays":self.config["retentionDays"]}));
        }
        let id = string(&a["id"], "job ID")?;
        let lease = string(&a["lease"], "lease")?;
        let Some(mut j) = self.get("jobs", id)? else {
            return Ok(json!({"cancel":true}));
        };
        if j["worker"] != worker["id"] || s(&j, "lease") != lease || s(&j, "state") != "reviewing" {
            return Ok(json!({"cancel":true}));
        }
        let runtime =
            if matches!(action, "heartbeat" | "report" | "failed") && !a["runtime"].is_null() {
                serde_json::to_value(crate::runtime_status::Progress::validate(&a["runtime"])?)?
            } else {
                j["runtime"].clone()
            };
        match action {
            "heartbeat" => {
                self.update(id, json!({"runtime":runtime}), Some(lease))?;
                Ok(json!({"cancel":false}))
            }
            "session" => {
                ensure!(valid_session(&a["session"]), "Invalid session identifier");
                self.update(id, json!({"session":a["session"]}), Some(lease))?;
                Ok(json!({"ok":true}))
            }
            "progress" => {
                self.update(id, json!({"retries":0}), Some(lease))?;
                Ok(json!({"ok":true}))
            }
            "comparison" => {
                let c = &a["comparison"];
                object(c)?;
                ensure!(
                    c["head"] == j["head"]
                        && c["target"] == j["target"]
                        && valid_sha(&c["base"])
                        && valid_sha(&c["targetSha"]),
                    "Invalid comparison"
                );
                let c = json!({"head":c["head"],"target":c["target"],"base":c["base"],"targetSha":c["targetSha"]});
                if !j["comparison"].is_null()
                    && comparison_key(&j["comparison"]) != comparison_key(&c)
                {
                    self.update(id, json!({"state":"superseded"}), Some(lease))?;
                    self.enqueue(
                        &self.repo(s(&j, "repo"))?,
                        n(&j, "number"),
                        &json!({"restart":true}),
                    )
                    .await?;
                    return Ok(json!({"cancel":true}));
                }
                let mut previous = self.get(
                    "completed",
                    &format!("{}:{}", s(&j, "key"), comparison_key(&c)),
                )?;
                let bot = self.config["app"]["botId"].as_i64();
                if previous.is_none() && !b(&j, "manual") && bot.is_some() {
                    let repo = self.repo(s(&j, "repo"))?;
                    let token = self.github.token(&repo).await?;
                    let reviews = self.github.reviews(&repo, n(&j, "number"), &token).await?;
                    previous = reviews
                        .iter()
                        .find(|r| {
                            r["user"]["id"].as_i64() == bot
                                && report::metadata(s(r, "body")).is_some_and(|m| {
                                    m["head"] == c["head"]
                                        && m["base"] == c["base"]
                                        && m["target"] == c["target"]
                                })
                        })
                        .map(|r| json!({"reviewUrl":r["html_url"]}));
                    if !self.owns(id, lease, "reviewing")? {
                        return Ok(json!({"cancel":true}));
                    }
                }
                if let Some(previous) = previous.filter(|_| !b(&j, "manual")) {
                    self.update(id,json!({"state":"completed","comparison":c,"reviewUrl":previous["reviewUrl"],"reason":null}),Some(lease))?;
                    return Ok(json!({"skip":true}));
                }
                for field in ["guidanceFingerprint", "guidanceTargetSha"] {
                    if let Some(v) = a.get(field) {
                        string(v, field)?;
                    }
                }
                self.db(|db| {
                    db.edit_job(id, Some(lease), |current| {
                        current["comparison"] = c.clone();
                        current["guidanceTargetSha"] = a
                            .get("guidanceTargetSha")
                            .unwrap_or(&c["targetSha"])
                            .clone();
                        // Undefined in the worker protocol means absent in saved
                        // jobs and review markers, never an explicit JSON null.
                        if let Some(fingerprint) = a.get("guidanceFingerprint") {
                            current["guidanceFingerprint"] = fingerprint.clone();
                        } else {
                            current
                                .as_object_mut()
                                .ok_or_else(|| anyhow!("Invalid job"))?
                                .remove("guidanceFingerprint");
                        }
                        Ok(())
                    })
                })?;
                Ok(json!({"ok":true}))
            }
            "report" => {
                ensure!(
                    !j["comparison"].is_null(),
                    "Comparison must be verified before a report is accepted"
                );
                let report = report::validate_report(&a["report"])?;
                let patch = s(a, "patch").chars().take(1000000).collect::<String>();
                self.update(
                    id,
                    json!({"report":report,"patch":patch,"state":"publishing","reason":null,"runtime":runtime}),
                    Some(lease),
                )?;
                Ok(json!({"ok":true}))
            }
            "failed" => {
                self.update(id, json!({"runtime":runtime}), Some(lease))?;
                if j["session"].is_null() && valid_session(&a["session"]) {
                    j["session"] = a["session"].clone();
                    self.update(id, json!({"session":a["session"]}), Some(lease))?;
                }
                ensure!(
                    !j["settings"].is_null(),
                    "Review settings are missing from the active job"
                );
                let kind = a["kind"].as_str().unwrap_or("transient");
                if kind == "superseded" {
                    let (pr, _) = self
                        .refresh(&self.repo(s(&j, "repo"))?, n(&j, "number"))
                        .await?;
                    let repo = self.repo(s(&j, "repo"))?;
                    if !self.owns(id, lease, "reviewing")? {
                        return Ok(json!({"cancel":true}));
                    }
                    self.update(id, json!({"state":"superseded"}), Some(lease))?;
                    if eligible(&repo, &pr) {
                        let replacement = self.db(|db| db.queue(&repo, &pr, &json!({})))?;
                        if pr["head"]["sha"] == j["head"] && pr["base"]["ref"] == j["target"] {
                            self.update(s(&replacement, "id"), json!({"nextAt":now()+5000}), None)?;
                        }
                    }
                    return Ok(json!({"cancel":true}));
                }
                let count = n(&j, "retries") + 1;
                let retry = &j["settings"]["retry"];
                let mut state = "paused";
                let mut reason =
                    "Review interrupted. Resume saved work or explicitly restart.".to_owned();
                let mut next_at = 0;
                if kind == "transient" && !j["session"].is_null() && count <= n(retry, "count") {
                    state = "retrying";
                    next_at = now()
                        + crate::store::retry_delay(
                            retry,
                            count as u64,
                            n(a, "retryAfter").clamp(0, 86400000) as u64,
                        ) as i64;
                    reason = format!(
                        "Provider interrupted; retry {count}/{} is scheduled.",
                        n(retry, "count")
                    );
                    self.db(|db| {
                        db.tx(|db| {
                            let cooldown = db
                                .get("state", "cooldown")?
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            db.put("state", "cooldown", &json!(cooldown.max(next_at)))
                        })
                    })?;
                }
                match kind {"auth"=>reason="Codex subscription login needs attention on the worker. Then resume this review.".to_owned(),"quota"=>reason="Subscription usage is unavailable. Resume when quota is available.".to_owned(),"config"=>reason="The configured model or reasoning level is unavailable. Check Crow settings on the worker.".to_owned(),_=>{}}
                if kind == "restart"
                    || (j["session"].is_null() && !["auth", "quota", "config"].contains(&kind))
                {
                    reason = "Restart required: no usable saved provider session.".to_owned();
                }
                if kind == "output" {
                    reason="Codex did not return a valid completed report. Resume the saved session to correct it.".to_owned();
                }
                self.update(
                    id,
                    json!({"state":state,"reason":reason,"retries":count,"nextAt":next_at}),
                    Some(lease),
                )?;
                Ok(json!({"ok":true}))
            }
            _ => bail!("Unknown worker action"),
        }
    }
}
fn number_argument(a: &Value) -> Result<i64> {
    let n = a["number"]
        .as_i64()
        .or_else(|| a["number"].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| anyhow!("Invalid pull request number"))?;
    ensure!(n > 0, "Invalid pull request number");
    Ok(n)
}
fn validate_admin(a: &Value) -> Result<()> {
    object(a)?;
    for field in [
        "id",
        "token",
        "repo",
        "githubToken",
        "worker",
        "model",
        "effort",
    ] {
        if let Some(v) = a.get(field) {
            string(v, field)?;
        }
    }
    if let Some(policy) = a.get("policy") {
        ensure!(
            ["selected", "everyone"].contains(&string(policy, "author policy")?),
            "Invalid author policy"
        );
    }
    for field in ["authors", "requesters"] {
        if let Some(v) = a.get(field) {
            ensure!(valid_logins(v), "Invalid {field}");
        }
    }
    for field in ["includeBacklog", "reenroll"] {
        if let Some(v) = a.get(field) {
            ensure!(v.is_boolean(), "Invalid {field}");
        }
    }
    if let Some(v) = a.get("settings") {
        config::parse_repository_settings(v)?;
    }
    Ok(())
}
fn optional_string(v: &Value, field: &str, out: &mut Value) -> Result<()> {
    if let Some(v) = v.get(field) {
        out[field] = json!(string(v, field)?);
    }
    Ok(())
}
fn extract_event(event_type: &str, payload: &Value) -> Result<Value> {
    if event_type == "ping" {
        return Ok(json!({"type":"ping"}));
    }
    object(payload)?;
    let repo = payload["repository"]["full_name"].as_str();
    let Some(repo) = repo else {
        return Ok(json!({"type":"ignored"}));
    };
    let mut out = json!({"type":event_type,"repo":repo});
    match event_type {
        "pull_request" => {
            if let Some(value) = payload.get("number") {
                ensure!(value.as_i64().is_some_and(|n| n > 0), "Invalid number");
                out["number"] = value.clone();
            }
            optional_string(payload, "action", &mut out)?;
            if let Some(time) = payload["pull_request"]["updated_at"].as_str()
                && let Ok(date) = chrono::DateTime::parse_from_rfc3339(time)
            {
                out["occurredAt"] = json!(date.timestamp_millis());
            }
        }
        "issue_comment"
            if !payload["issue"]["pull_request"].is_null() && s(payload, "action") == "created" =>
        {
            if let Some(value) = payload["issue"].get("number") {
                ensure!(value.as_i64().is_some_and(|n| n > 0), "Invalid number");
                out["number"] = value.clone();
            }
            let comment = &payload["comment"];
            let command = comment["body"].as_str().unwrap_or("").trim();
            let command = command
                .strip_prefix("/crow ")
                .or_else(|| command.strip_prefix("@crow "))
                .filter(|c| ["review", "resume", "restart", "pause"].contains(c));
            out["request"] = json!(command.is_some());
            if let Some(command) = command {
                out["command"] = json!(command);
            }
            if let Some(actor) = comment["user"]["login"].as_str() {
                out["actor"] = json!(actor);
            }
            if let Some(time) = comment["created_at"].as_str()
                && let Ok(date) = chrono::DateTime::parse_from_rfc3339(time)
            {
                out["occurredAt"] = json!(date.timestamp_millis());
            }
        }
        "push" => optional_string(payload, "ref", &mut out)?,
        _ => return Ok(json!({"type":"ignored"})),
    }
    Ok(out)
}
fn response(code: u16, body: Value) -> Response {
    let mut response = (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        axum::Json(body),
    )
        .into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}
async fn read_body(body: Body, shutdown: &CancellationToken) -> Result<axum::body::Bytes> {
    tokio::select! { biased;
        _ = shutdown.cancelled() => bail!("Service is shutting down"),
        bytes = to_bytes(body, 2 * 1024 * 1024) => bytes.map_err(|_| anyhow!("Request too large")),
    }
}
fn parse_body(bytes: &[u8]) -> Result<Value> {
    let mut value: Value = serde_json::from_slice(bytes)?;
    config::normalize_numbers(&mut value);
    Ok(value)
}
async fn handler(State(service): State<Arc<Service>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let method = parts.method.as_str();
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
    if method == "GET" && path == "/health" {
        return response(
            200,
            json!({"ok":true,"service":"crow","configured":!service.config["app"].is_null()}),
        );
    }
    let work = async {
        if method == "POST" && path == "/webhooks/github" {
            let raw = read_body(body, &service.shutdown).await?;
            if !crate::github::verify(
                &raw,
                header("x-hub-signature-256"),
                service.config["app"]["webhookSecret"].as_str(),
            ) {
                return Ok((401, json!({"error":"Invalid webhook signature"})));
            }
            let Some(delivery) =
                header("x-github-delivery").filter(|id| !id.is_empty() && id.len() <= 200)
            else {
                return Ok((400, json!({"error":"Missing delivery ID"})));
            };
            let payload = parse_body(&raw)?;
            let extracted = extract_event(header("x-github-event").unwrap_or(""), &payload)?;
            if let Some(name) = extracted["repo"].as_str() {
                let enrolled = service.get("repos", &util::repo_name(name)?)?;
                if enrolled.as_ref().is_none_or(|repo| {
                    payload["installation"]
                        .get("id")
                        .is_some_and(|id| id != &repo["installation"])
                }) {
                    return Ok((202, json!({"accepted":false})));
                }
            }
            let accepted = service.db(|db| db.accept_event(delivery, &extracted))?;
            return Ok((202, json!({"accepted":accepted})));
        }
        let bearer = header("authorization")
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .unwrap_or(header("authorization").unwrap_or(""));
        if let Some(action) = path.strip_prefix("/admin/") {
            if !util::equal(bearer, s(&service.config, "adminToken")) {
                return Ok((401, json!({"error":"Unauthorized"})));
            }
            if action == "status" && method == "GET" {
                let workers = service
                    .all("workers")?
                    .into_iter()
                    .map(|mut w| {
                        w.as_object_mut().unwrap().remove("token");
                        w
                    })
                    .collect::<Vec<_>>();
                let jobs = service
                    .all("jobs")?
                    .into_iter()
                    .map(|mut j| {
                        for key in ["report", "lease", "patch"] {
                            j.as_object_mut().unwrap().remove(key);
                        }
                        j
                    })
                    .collect::<Vec<_>>();
                return Ok((
                    200,
                    json!({"repos":service.all("repos")?,"workers":workers,"jobs":jobs,"draining":service.get("state","drain")?.is_some_and(|x|x.as_bool()==Some(true))}),
                ));
            }
            if method != "POST" {
                return Ok((405, json!({"error":"POST required"})));
            }
            let raw = read_body(body, &service.shutdown).await?;
            return Ok((200, service.admin(action, &parse_body(&raw)?).await?));
        }
        if let Some(action) = path.strip_prefix("/worker/").filter(|_| method == "POST") {
            let worker = service
                .all("workers")?
                .into_iter()
                .find(|w| util::equal(s(w, "token"), bearer));
            let Some(worker) = worker else {
                return Ok((401, json!({"error":"Unauthorized worker"})));
            };
            let raw = read_body(body, &service.shutdown).await?;
            return Ok((
                200,
                service
                    .worker_action(action, &parse_body(&raw)?, worker)
                    .await?,
            ));
        }
        Ok((404, json!({"error":"Not found"})))
    };
    let result: Result<(u16, Value)> = work.await;
    match result {
        Ok((status, body)) => response(status, body),
        Err(e) => {
            eprintln!("Request failed: {e}");
            response(
                if e.to_string() == "Request too large" {
                    413
                } else if e.to_string() == "Service is shutting down" {
                    503
                } else {
                    error_status(&e)
                },
                json!({"error":e.to_string()}),
            )
        }
    }
}

// Every state-machine future runs on one owned current-thread runtime. This
// preserves the atomic synchronous sections of the original event loop while
// GitHub requests remain concurrent. No database lock survives an await.
type ServiceCommand = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Running service. Administrative calls cross a channel into its owned runtime.
pub struct ServiceHandle {
    service: Arc<Service>,
    address: SocketAddr,
    cancel: CancellationToken,
    commands: tokio::sync::mpsc::UnboundedSender<ServiceCommand>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}
impl ServiceHandle {
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    fn execute<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>> {
        let commands = self.commands.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            ensure!(!cancel.is_cancelled(), "Service is shutting down");
            let (send, receive) = tokio::sync::oneshot::channel();
            commands
                .send(Box::pin(async move {
                    let _ = send.send(work.await);
                }))
                .map_err(|_| anyhow!("Service is closed"))?;
            receive.await.map_err(|_| anyhow!("Service is closed"))?
        })
    }
    pub async fn admin(&self, action: &str, arguments: &Value) -> Result<Value> {
        let service = self.service.clone();
        let action = action.to_owned();
        let mut arguments = arguments.clone();
        config::normalize_numbers(&mut arguments);
        self.execute(async move { service.admin(&action, &arguments).await })
            .await
    }
    pub async fn close(mut self) -> Result<()> {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || {
                thread
                    .join()
                    .map_err(|_| anyhow!("Service runtime panicked"))?
            })
            .await??;
        }
        Ok(())
    }
}
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct RunningService {
    service: Arc<Service>,
    address: SocketAddr,
    cancel: CancellationToken,
    server: JoinHandle<Result<()>>,
    tasks: Vec<JoinHandle<()>>,
}
impl RunningService {
    async fn close(self) -> Result<()> {
        self.cancel.cancel();
        let mut error = match self.server.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e.into()),
        };
        for task in self.tasks {
            if let Err(e) = task.await {
                error.get_or_insert(e.into());
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        Ok(())
    }
}
pub async fn start_service(config: Value, root: PathBuf) -> Result<ServiceHandle> {
    let app = config.get("app").filter(|app| !app.is_null()).cloned();
    start_service_with_github(config, root, Arc::new(GitHub::new(app))).await
}
pub async fn start_service_with_github(
    config: Value,
    root: PathBuf,
    github: Arc<dyn GitHubApi>,
) -> Result<ServiceHandle> {
    let (started, ready) = tokio::sync::oneshot::channel();
    let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel::<ServiceCommand>();
    let thread = std::thread::Builder::new().name("crow-service".into()).spawn(move || -> Result<()> {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => { let _ = started.send(Err(anyhow!(error))); return Ok(()); }
        };
        runtime.block_on(async move {
            let running = match start_on_runtime(config, root, github).await {
                Ok(running) => running,
                Err(error) => { let _ = started.send(Err(error)); return Ok(()); }
            };
            let cancel = running.cancel.clone();
            if started.send(Ok((running.service.clone(), running.address, cancel.clone()))).is_err() { return running.close().await; }
            let mut calls = tokio::task::JoinSet::new();
            loop {
                tokio::select! { biased;
                    _ = cancel.cancelled() => break,
                    command = receiver.recv() => match command { Some(command) => { calls.spawn(command); }, None => break },
                    _ = calls.join_next(), if !calls.is_empty() => {},
                }
            }
            cancel.cancel();
            while calls.join_next().await.is_some() {}
            running.close().await
        })
    })?;
    match ready.await {
        Ok(Ok((service, address, cancel))) => Ok(ServiceHandle {
            service,
            address,
            cancel,
            commands,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(error) => {
            let _ = thread.join();
            Err(error.into())
        }
    }
}
async fn start_on_runtime(
    mut config: Value,
    root: PathBuf,
    github: Arc<dyn GitHubApi>,
) -> Result<RunningService> {
    config::normalize_numbers(&mut config);
    let store = Store::new(&root.join("service.sqlite"))?;
    if s(&config, "role") == "both" {
        let old = store
            .get("workers", s(&config["worker"], "id"))?
            .unwrap_or_else(|| json!({}));
        store.put(
            "workers",
            s(&config["worker"], "id"),
            &patch(
                old,
                &json!({"id":config["worker"]["id"],"token":config["worker"]["token"]}),
            ),
        )?;
    }
    let restored = util::read_json(&root.join("restore-pending.json"))?.is_some();
    for j in store.all("jobs")?.iter().filter(|j| {
        s(j, "state") == "reviewing"
            || (restored && ["queued", "retrying"].contains(&s(j, "state")))
    }) {
        store.update_job(s(j,"id"),&json!({"state":"paused","autoRecover":!restored,"reason":if restored{"Backup restored. Resume after checking worker availability."}else{"Service restarted; waiting for the worker to reconnect."}}),None)?;
    }
    if restored {
        std::fs::remove_file(root.join("restore-pending.json"))?;
    }
    let listener =
        tokio::net::TcpListener::bind((s(&config, "bind"), n(&config, "port") as u16)).await?;
    let address = listener.local_addr()?;
    config["port"] = json!(address.port());
    let cancel = CancellationToken::new();
    let service = Arc::new(Service {
        config: config.clone(),
        store: Mutex::new(store),
        github,
        scans: Mutex::new(HashMap::new()),
        shutdown: cancel.clone(),
    });
    let shutdown = cancel.clone();
    let app = Router::new()
        .fallback(any(handler))
        .with_state(service.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await?;
        Ok(())
    });
    let mut tasks = Vec::new();
    for kind in 0..3 {
        let service = service.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let duration=if kind==2{n(&service.config,"auditIntervalMs").max(1) as u64}else{1000};
            let mut interval=tokio::time::interval(Duration::from_millis(duration));interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Event and publication processing begin on the first scheduled tick.
            if kind!=2 {interval.tick().await;}
            loop {tokio::select! {biased; _=cancel.cancelled()=>break,_=interval.tick()=>{let result=match kind {0=>service.tick().await,1=>service.publish_tick().await,_=>service.audit().await};if let Err(e)=result{eprintln!("Service task deferred: {e}");}}}}
        }));
    }
    if b(&config["catchUp"], "enabled") {
        for repo in service.all("repos")? {
            let service = service.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = service.catch_up(s(&repo, "name"), false).await {
                    eprintln!("Catch-up deferred: {e}");
                }
            }));
        }
    }
    Ok(RunningService {
        service,
        address,
        cancel,
        server,
        tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    #[test]
    fn pr_comment_commands_accept_slash_and_legacy_prefixes() {
        for prefix in ["/crow", "@crow"] {
            for command in ["review", "resume", "restart", "pause"] {
                let payload = json!({
                    "action": "created",
                    "repository": {"full_name": "owner/project"},
                    "issue": {"number": 1, "pull_request": {}},
                    "comment": {
                        "body": format!(" \n{prefix} {command}\n "),
                        "user": {"login": "alice"}
                    }
                });
                let event = extract_event("issue_comment", &payload).unwrap();
                assert_eq!(event["request"], true);
                assert_eq!(event["command"], command);
                assert_eq!(event["actor"], "alice");
                assert_eq!(event["number"], 1);
            }
        }
    }

    #[test]
    fn pr_comment_commands_ignore_prose_quotes_and_other_events() {
        for prefix in ["/crow", "@crow"] {
            let mut payload = json!({
                "action": "created",
                "repository": {"full_name": "owner/project"},
                "issue": {"number": 1, "pull_request": {}},
                "comment": {"user": {"login": "alice"}}
            });
            for body in [
                prefix.to_owned(),
                format!("{prefix} unknown"),
                format!("{prefix} review please"),
                format!("Please run {prefix} review"),
                format!("{prefix} review\n{prefix} pause"),
                format!("`{prefix} pause`"),
                format!("```\n{prefix} pause\n```"),
                format!("> {prefix} pause"),
            ] {
                payload["comment"]["body"] = json!(body);
                let event = extract_event("issue_comment", &payload).unwrap();
                assert_eq!(event["request"], false, "{body}");
                assert!(event.get("command").is_none(), "{body}");
            }
            payload["comment"]["body"] = json!(format!("{prefix} review"));
            payload["action"] = json!("edited");
            assert_eq!(
                extract_event("issue_comment", &payload).unwrap()["type"],
                "ignored"
            );
            payload["action"] = json!("created");
            payload["issue"]["pull_request"] = Value::Null;
            assert_eq!(
                extract_event("issue_comment", &payload).unwrap()["type"],
                "ignored"
            );
        }
    }

    struct Gate {
        entered: Semaphore,
        release: Semaphore,
    }
    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: Semaphore::new(0),
                release: Semaphore::new(0),
            })
        }
        async fn block(&self) {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
        async fn wait(&self) {
            tokio::time::timeout(Duration::from_secs(3), self.entered.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
        fn open(&self) {
            self.release.add_permits(1);
        }
    }
    #[derive(Default)]
    struct FakeGitHub {
        prs: Mutex<Vec<Value>>,
        reviews: Mutex<Vec<Value>>,
        published: Mutex<Vec<Value>>,
        statuses: Mutex<Vec<String>>,
        pr_gate: Mutex<Option<Arc<Gate>>>,
        prs_gate: Mutex<Option<Arc<Gate>>>,
        publish_gate: Mutex<Option<Arc<Gate>>>,
        fail_publish: AtomicBool,
        fail_pr: AtomicBool,
        pr_calls: AtomicUsize,
        threads: Mutex<HashSet<std::thread::ThreadId>>,
    }
    #[async_trait]
    impl GitHubApi for FakeGitHub {
        async fn request(
            &self,
            path: &str,
            _: Option<&str>,
            _: &str,
            _: Option<&Value>,
        ) -> Result<Value> {
            Ok(if path == "/user" {
                json!({"login":"alice"})
            } else {
                json!({"permissions":{"maintain":true}})
            })
        }
        async fn token(&self, _: &Value) -> Result<String> {
            self.threads
                .lock()
                .unwrap()
                .insert(std::thread::current().id());
            Ok("installation-token".to_owned())
        }
        async fn checkout_token(&self, _: &Value) -> Result<String> {
            Ok("checkout-token".to_owned())
        }
        async fn pr(&self, _: &Value, number: i64, _: &str) -> Result<Value> {
            self.pr_calls.fetch_add(1, Ordering::Relaxed);
            let pr = self
                .prs
                .lock()
                .unwrap()
                .iter()
                .find(|p| n(p, "number") == number)
                .cloned()
                .ok_or_else(|| anyhow!("missing PR"))?;
            let gate = self.pr_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.block().await;
            }
            if self.fail_pr.swap(false, Ordering::Relaxed) {
                bail!("GitHub unavailable");
            }
            Ok(pr)
        }
        async fn prs(&self, _: &Value, _: &str) -> Result<Vec<Value>> {
            let prs = self.prs.lock().unwrap().clone();
            let gate = self.prs_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.block().await;
            }
            Ok(prs)
        }
        async fn reviews(&self, _: &Value, _: i64, _: &str) -> Result<Vec<Value>> {
            Ok(self.reviews.lock().unwrap().clone())
        }
        async fn installation(&self, _: &str) -> Result<Value> {
            Ok(json!({"id":11}))
        }
        async fn status(
            &self,
            _: &Value,
            _: i64,
            _: &str,
            body: &str,
            _: i64,
            _: Option<i64>,
        ) -> Result<Value> {
            self.statuses.lock().unwrap().push(body.to_owned());
            Ok(json!({"id":1}))
        }
        async fn publish(
            &self,
            _: &Value,
            number: i64,
            _: &str,
            body: &str,
            head: &str,
            comments: &[Value],
        ) -> Result<Value> {
            let gate = self.publish_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.block().await;
            }
            if self.fail_publish.swap(false, Ordering::Relaxed) {
                bail!("GitHub unavailable");
            }
            self.published
                .lock()
                .unwrap()
                .push(json!({"number":number,"body":body,"head":head,"comments":comments}));
            Ok(
                json!({"id":1,"html_url":"https://github.com/owner/project/pull/1#pullrequestreview-1"}),
            )
        }
        async fn audit(&self) -> Result<()> {
            Ok(())
        }
    }
    fn pr(number: i64) -> Value {
        json!({"number":number,"state":"open","draft":false,"user":{"login":"alice"},"head":{"sha":"a".repeat(40)},"base":{"ref":"main","sha":"c".repeat(40)}})
    }
    fn comparison() -> Value {
        json!({"head":"a".repeat(40),"base":"b".repeat(40),"target":"main","targetSha":"c".repeat(40)})
    }
    struct Fixture {
        root: tempfile::TempDir,
        handle: ServiceHandle,
        github: Arc<FakeGitHub>,
        config: Value,
    }
    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let mut config = config::defaults(root.path());
            config["port"] = json!(0);
            config["operator"] = json!("alice");
            config["catchUp"]["enabled"] = json!(false);
            config["app"] =
                json!({"id":1,"slug":"crow-test","pem":"test","webhookSecret":"secret","botId":42});
            let github = Arc::new(FakeGitHub::default());
            github.prs.lock().unwrap().push(pr(1));
            let handle =
                start_service_with_github(config.clone(), root.path().to_owned(), github.clone())
                    .await
                    .unwrap();
            let repo = json!({"name":"owner/project","installation":11,"worker":config["worker"]["id"],"policy":"selected","authors":["alice"],"requesters":["alice"],"settings":{},"enrolledAt":0,"excluded":[]});
            handle
                .service
                .db(|db| db.enroll(&repo).map(|_| ()))
                .unwrap();
            Self {
                root,
                handle,
                github,
                config,
            }
        }
        async fn worker(&self, action: &str, args: Value) -> Result<Value> {
            let service = self.handle.service.clone();
            let action = action.to_owned();
            let id = s(&self.config["worker"], "id").to_owned();
            self.handle
                .execute(async move {
                    let worker = service.get("workers", &id)?.unwrap();
                    service.worker_action(&action, &args, worker).await
                })
                .await
        }
        async fn queue(&self) -> Value {
            let service = self.handle.service.clone();
            self.handle
                .execute(async move {
                    service
                        .enqueue(&service.repo("owner/project")?, 1, &json!({}))
                        .await
                })
                .await
                .unwrap()
        }
        async fn publish_tick(&self) {
            let service = self.handle.service.clone();
            self.handle
                .execute(async move { service.publish_tick().await })
                .await
                .unwrap();
        }
        async fn catch_up(&self, include: bool) -> Value {
            let service = self.handle.service.clone();
            self.handle
                .execute(async move { service.catch_up("owner/project", include).await })
                .await
                .unwrap()
        }
        async fn claim(&self) -> Value {
            self.worker("next", json!({"active":[]})).await.unwrap()["job"].clone()
        }
        fn job(&self, id: &str) -> Value {
            self.handle.service.get("jobs", id).unwrap().unwrap()
        }
        async fn prepared(&self) -> Value {
            self.queue().await;
            let j = self.claim().await;
            self.worker(
                "comparison",
                json!({"id":j["id"],"lease":j["lease"],"comparison":comparison()}),
            )
            .await
            .unwrap();
            j
        }
        async fn report(&self, j: &Value) {
            self.worker("report",json!({"id":j["id"],"lease":j["lease"],"report":{"summary":"No issues found.","findings":[]},"patch":"private patch"})).await.unwrap();
        }
        async fn call(&self, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
            let client = reqwest::Client::new();
            let request = if let Some(body) = body {
                client
                    .post(format!("http://{}{path}", self.handle.address()))
                    .json(&body)
            } else {
                client.get(format!("http://{}{path}", self.handle.address()))
            };
            let r = request.bearer_auth(token).send().await.unwrap();
            (r.status().as_u16(), r.json().await.unwrap())
        }
        async fn close(self) {
            self.handle.close().await.unwrap();
            drop(self.root);
        }
    }
    #[tokio::test]
    async fn runtime_heartbeat_updates_main_comment_and_final_report_is_durable() {
        let f = Fixture::new().await;
        let job = f.prepared().await;
        let mut runtime = crate::runtime_status::Progress {
            enabled: true,
            ..Default::default()
        };
        runtime.setup.running = 1;
        for (phase, expected) in [(0, "Setting up the test environment"), (1, "Running tests")] {
            if phase == 1 {
                runtime.setup.running = 0;
                runtime.setup.passed = 1;
                runtime.tests.running = 1;
            }
            f.worker(
                "heartbeat",
                json!({"id":job["id"],"lease":job["lease"],"runtime":runtime}),
            )
            .await
            .unwrap();
            let service = f.handle.service.clone();
            let id = s(&job, "id").to_owned();
            f.handle
                .execute(async move { service.status(&service.get("jobs", &id)?.unwrap()).await })
                .await
                .unwrap();
            assert!(
                f.github
                    .statuses
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .contains(expected)
            );
        }
        let before = f.job(s(&job, "id"))["runtime"].clone();
        assert_eq!(
            f.worker(
                "heartbeat",
                json!({"id":job["id"],"lease":"stale-lease","runtime":{}})
            )
            .await
            .unwrap()["cancel"],
            true
        );
        assert_eq!(f.job(s(&job, "id"))["runtime"], before);
        let mut invalid = json!(runtime);
        invalid["tests"]["running"] = json!(1000);
        assert!(
            f.worker(
                "heartbeat",
                json!({"id":job["id"],"lease":job["lease"],"runtime":invalid})
            )
            .await
            .is_err()
        );
        runtime.tests.running = 0;
        runtime.tests.failed = 1;
        f.worker("report",json!({"id":job["id"],"lease":job["lease"],"runtime":runtime,"report":{"summary":"A test failed on base and head.","findings":[]}})).await.unwrap();
        // A late heartbeat cannot replace the final counters after publication starts.
        assert_eq!(
            f.worker(
                "heartbeat",
                json!({"id":job["id"],"lease":job["lease"],"runtime":before})
            )
            .await
            .unwrap()["cancel"],
            true
        );
        assert_eq!(f.job(s(&job, "id"))["runtime"], json!(runtime));
        f.publish_tick().await;
        let service = f.handle.service.clone();
        let id = s(&job, "id").to_owned();
        f.handle
            .execute(async move { service.status(&service.get("jobs", &id)?.unwrap()).await })
            .await
            .unwrap();
        let body = f.github.statuses.lock().unwrap().last().unwrap().clone();
        assert!(body.contains("Finished. Test commands: 1 failed."));
        assert!(body.contains("base failures"));
        f.close().await;
    }

    #[tokio::test]
    async fn workers_receive_only_their_assigned_jobs_and_checkout_tokens() {
        let f = Fixture::new().await;
        f.handle
            .admin(
                "pair",
                &json!({"id":"second-worker","token":"s".repeat(64)}),
            )
            .await
            .unwrap();
        let mut other_repo = f.handle.service.repo("owner/project").unwrap();
        other_repo["name"] = json!("owner/other");
        other_repo["worker"] = json!("second-worker");
        f.handle
            .service
            .db(|db| db.enroll(&other_repo).map(|_| ()))
            .unwrap();
        f.queue().await;
        f.handle
            .service
            .enqueue(&other_repo, 1, &json!({}))
            .await
            .unwrap();
        let (_, first) = f
            .call(
                "/worker/next",
                s(&f.config["worker"], "token"),
                Some(json!({"active":[]})),
            )
            .await;
        let (_, second) = f
            .call("/worker/next", &"s".repeat(64), Some(json!({"active":[]})))
            .await;
        assert_eq!(first["job"]["repo"], "owner/project");
        assert_eq!(second["job"]["repo"], "owner/other");
        for work in [&first, &second] {
            assert_eq!(work["token"], "checkout-token");
            assert!(!work.to_string().contains("installation-token"));
        }
        let (_, stolen) = f
            .call(
                "/worker/heartbeat",
                &"s".repeat(64),
                Some(json!({"id":first["job"]["id"],"lease":first["job"]["lease"]})),
            )
            .await;
        assert_eq!(stolen["cancel"], true);
        let (_, next) = f
            .call("/worker/next", &"s".repeat(64), Some(json!({"active":[]})))
            .await;
        assert!(next.is_null());
        f.close().await;
    }
    #[tokio::test]
    async fn http_auth_health_and_status_redaction() {
        let f = Fixture::new().await;
        assert_eq!(
            f.call("/health", "", None).await,
            (200, json!({"ok":true,"service":"crow","configured":true}))
        );
        assert_eq!(
            f.call("/admin/status", s(&f.config["worker"], "token"), None)
                .await
                .0,
            401
        );
        assert_eq!(
            f.call("/worker/ping", s(&f.config, "adminToken"), Some(json!({})))
                .await
                .0,
            401
        );
        let j = f.prepared().await;
        f.report(&j).await;
        let (status, data) = f
            .call("/admin/status", s(&f.config, "adminToken"), None)
            .await;
        assert_eq!(status, 200);
        let encoded = data.to_string();
        assert!(!encoded.contains("private patch"));
        assert!(!encoded.contains(s(&f.config["worker"], "token")));
        assert!(data["jobs"][0].get("lease").is_none());
        assert!(data["jobs"][0].get("report").is_none());
        assert_eq!(
            f.call("/admin/drain", s(&f.config, "adminToken"), None)
                .await
                .0,
            405
        );
        f.close().await;
    }
    #[tokio::test]
    async fn webhook_is_durable_minimal_authenticated_and_deduplicated() {
        let f = Fixture::new().await;
        let payload = json!({"action":"opened","number":1,"repository":{"full_name":"owner/project"},"installation":{"id":11},"pull_request":{"body":"private full PR text"}});
        let raw = payload.to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(raw.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let client = reqwest::Client::new();
        for expected in [true, false] {
            let r = client
                .post(format!("http://{}/webhooks/github", f.handle.address()))
                .header("x-hub-signature-256", &signature)
                .header("x-github-delivery", "delivery-1")
                .header("x-github-event", "pull_request")
                .body(raw.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 202);
            assert_eq!(r.json::<Value>().await.unwrap()["accepted"], expected);
        }
        let events = f.handle.service.db(|db| db.events()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(!json!(events).to_string().contains("private full PR text"));
        let r = client
            .post(format!("http://{}/webhooks/github", f.handle.address()))
            .header("x-hub-signature-256", signature)
            .header("x-github-delivery", "delivery-2")
            .body(format!("{raw} "))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        f.close().await;
    }
    #[tokio::test]
    async fn supersession_invalidates_old_lease_and_waits_for_running_process() {
        let f = Fixture::new().await;
        let old = f.queue().await;
        let claimed = f.claim().await;
        f.github.prs.lock().unwrap()[0]["head"]["sha"] = json!("d".repeat(40));
        let new = f.queue().await;
        assert_ne!(old["id"], new["id"]);
        assert_eq!(f.job(s(&old, "id"))["state"], "superseded");
        assert_eq!(
            f.worker(
                "heartbeat",
                json!({"id":claimed["id"],"lease":claimed["lease"]})
            )
            .await
            .unwrap()["cancel"],
            true
        );
        assert!(
            f.worker("next", json!({"active":[old["id"]]}))
                .await
                .unwrap()
                .is_null()
        );
        assert_eq!(f.claim().await["id"], new["id"]);
        f.close().await;
    }
    #[tokio::test]
    async fn comparison_without_guidance_remains_resumable_and_reconcilable() {
        let f = Fixture::new().await;
        let job = f.prepared().await;
        assert!(f.job(s(&job, "id")).get("guidanceFingerprint").is_none());
        f.handle
            .service
            .update(s(&job, "id"), json!({"state":"queued"}), None)
            .unwrap();
        let dispatch = f.worker("next", json!({"active":[]})).await.unwrap();
        crate::worker::validate_response(
            &config::defaults(f.root.path()),
            "next",
            dispatch.clone(),
        )
        .unwrap();
        let resumed = &dispatch["job"];
        f.report(resumed).await;
        f.publish_tick().await;
        let published = f.github.published.lock().unwrap()[0].clone();
        let metadata = report::metadata(s(&published, "body")).unwrap();
        assert_eq!(metadata["job"], job["id"]);
        assert!(metadata.get("guidance").is_none());

        // A lost acknowledgment must reconcile the authenticated review marker
        // instead of publishing the same completed report a second time.
        f.github.reviews.lock().unwrap().push(json!({
            "user":{"id":42},"body":published["body"],
            "html_url":"https://github.com/owner/project/pull/1#pullrequestreview-1"
        }));
        f.handle
            .service
            .update(s(&job, "id"), json!({"state":"publishing"}), None)
            .unwrap();
        f.publish_tick().await;
        assert_eq!(f.job(s(&job, "id"))["state"], "completed");
        assert_eq!(f.github.published.lock().unwrap().len(), 1);
        f.close().await;
    }

    #[tokio::test]
    async fn comparison_omission_clears_previous_guidance_but_null_is_invalid() {
        let f = Fixture::new().await;
        let job = f.prepared().await;
        let mut args = json!({"id":job["id"],"lease":job["lease"],"comparison":comparison(),"guidanceFingerprint":"saved-guidance"});
        f.worker("comparison", args.clone()).await.unwrap();
        assert_eq!(
            f.job(s(&job, "id"))["guidanceFingerprint"],
            "saved-guidance"
        );
        args["guidanceFingerprint"] = Value::Null;
        assert!(f.worker("comparison", args.clone()).await.is_err());
        assert_eq!(
            f.job(s(&job, "id"))["guidanceFingerprint"],
            "saved-guidance"
        );
        args.as_object_mut().unwrap().remove("guidanceFingerprint");
        f.worker("comparison", args).await.unwrap();
        assert!(f.job(s(&job, "id")).get("guidanceFingerprint").is_none());
        assert_eq!(
            f.job(s(&job, "id"))["guidanceTargetSha"],
            comparison()["targetSha"]
        );
        f.close().await;
    }

    #[tokio::test]
    async fn report_requires_comparison_and_is_durable_before_publication() {
        let f = Fixture::new().await;
        f.queue().await;
        let j = f.claim().await;
        let args = json!({"id":j["id"],"lease":j["lease"],"report":{"summary":"No issues.","findings":[]}});
        assert!(
            f.worker("report", args.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("Comparison")
        );
        f.worker(
            "comparison",
            json!({"id":j["id"],"lease":j["lease"],"comparison":comparison()}),
        )
        .await
        .unwrap();
        f.worker("report", args).await.unwrap();
        assert_eq!(f.job(s(&j, "id"))["state"], "publishing");
        assert!(f.github.published.lock().unwrap().is_empty());
        f.github.fail_publish.store(true, Ordering::Relaxed);
        f.publish_tick().await;
        let saved = f.job(s(&j, "id"));
        assert_eq!(saved["state"], "publishing");
        assert!(!saved["report"].is_null());
        assert!(f.claim().await.is_null());
        f.handle
            .service
            .update(s(&j, "id"), json!({"publishAt":0}), None)
            .unwrap();
        f.publish_tick().await;
        assert_eq!(f.job(s(&j, "id"))["state"], "completed");
        assert_eq!(f.github.published.lock().unwrap().len(), 1);
        f.close().await;
    }
    #[tokio::test]
    async fn catch_up_cancels_missing_pr_and_holds_large_backlog() {
        let f = Fixture::new().await;
        let j = f.queue().await;
        f.github.prs.lock().unwrap().clear();
        assert_eq!(f.catch_up(false).await, json!({"queued":0,"held":0}));
        assert_eq!(f.job(s(&j, "id"))["state"], "cancelled");
        assert_eq!(f.job(s(&j, "id"))["reason"], "PR is closed");
        f.github.prs.lock().unwrap().extend((2..=13).map(pr));
        assert_eq!(f.catch_up(false).await["held"], 12);
        assert_eq!(f.catch_up(false).await["held"], 0);
        assert!(f.claim().await.is_null());
        assert_eq!(
            f.handle.admin("release", &json!({})).await.unwrap()["released"],
            12
        );
        assert!(!f.claim().await.is_null());
        f.close().await;
    }
    #[tokio::test]
    async fn authorization_revoked_during_claim_cancels_dispatch() {
        let f = Fixture::new().await;
        let j = f.queue().await;
        let gate = Gate::new();
        *f.github.pr_gate.lock().unwrap() = Some(gate.clone());
        let service = f.handle.service.clone();
        let worker = service
            .get("workers", s(&f.config["worker"], "id"))
            .unwrap()
            .unwrap();
        let claim = tokio::spawn(f.handle.execute(async move {
            service
                .worker_action("next", &json!({"active":[]}), worker)
                .await
        }));
        gate.wait().await;
        f.handle
            .admin(
                "config-repo",
                &json!({"repo":"owner/project","authors":["bob"]}),
            )
            .await
            .unwrap();
        gate.open();
        assert!(claim.await.unwrap().unwrap().is_null());
        assert_eq!(f.job(s(&j, "id"))["state"], "superseded");
        f.close().await;
    }
    #[tokio::test]
    async fn stale_failed_claim_cannot_resurrect_superseded_job() {
        let f = Fixture::new().await;
        let old = f.queue().await;
        let gate = Gate::new();
        *f.github.pr_gate.lock().unwrap() = Some(gate.clone());
        let service = f.handle.service.clone();
        let worker = service
            .get("workers", s(&f.config["worker"], "id"))
            .unwrap()
            .unwrap();
        let claim = tokio::spawn(f.handle.execute(async move {
            service
                .worker_action("next", &json!({"active":[]}), worker)
                .await
        }));
        gate.wait().await;
        f.github.prs.lock().unwrap()[0]["head"]["sha"] = json!("d".repeat(40));
        f.queue().await;
        f.github.fail_pr.store(true, Ordering::Relaxed);
        gate.open();
        assert!(claim.await.unwrap().is_err());
        assert_eq!(f.job(s(&old, "id"))["state"], "superseded");
        f.close().await;
    }
    #[tokio::test]
    async fn publication_race_preserves_pause_and_supersession() {
        for pause in [true, false] {
            let f = Fixture::new().await;
            let j = f.prepared().await;
            f.report(&j).await;
            let gate = Gate::new();
            *f.github.publish_gate.lock().unwrap() = Some(gate.clone());
            let service = f.handle.service.clone();
            let publish = tokio::spawn(
                f.handle
                    .execute(async move { service.publish_tick().await }),
            );
            gate.wait().await;
            if pause {
                f.handle
                    .admin("pause", &json!({"repo":"owner/project","number":1}))
                    .await
                    .unwrap();
            } else {
                f.github.prs.lock().unwrap()[0]["head"]["sha"] = json!("d".repeat(40));
                f.queue().await;
            }
            gate.open();
            publish.await.unwrap().unwrap();
            let saved = f.job(s(&j, "id"));
            assert_eq!(saved["state"], if pause { "paused" } else { "superseded" });
            assert!(saved["reviewUrl"].is_string());
            f.close().await;
        }
    }
    #[tokio::test]
    async fn reconnect_resumes_saved_sessions_but_backup_restore_requires_operator() {
        let f = Fixture::new().await;
        f.queue().await;
        let j = f.claim().await;
        let root = f.root.path().to_owned();
        let config = f.config.clone();
        let github = f.github.clone();
        let id = s(&j, "id").to_owned();
        f.worker("session",json!({"id":j["id"],"lease":j["lease"],"session":"12345678-abcd-abcd-abcd-123456789012"})).await.unwrap();
        f.handle.close().await.unwrap();
        let handle = start_service_with_github(config.clone(), root.clone(), github.clone())
            .await
            .unwrap();
        let paused = handle.service.get("jobs", &id).unwrap().unwrap();
        assert_eq!(paused["state"], "paused");
        assert_eq!(paused["autoRecover"], true);
        let worker = handle
            .service
            .get("workers", s(&config["worker"], "id"))
            .unwrap()
            .unwrap();
        let service = handle.service.clone();
        let resumed = handle
            .execute(async move {
                service
                    .worker_action("next", &json!({"active":[]}), worker)
                    .await
            })
            .await
            .unwrap();
        assert_eq!(resumed["job"]["id"], id);
        handle.close().await.unwrap();
        util::atomic(
            &root.join("restore-pending.json"),
            &json!({"restored":true}),
        )
        .unwrap();
        let handle = start_service_with_github(config.clone(), root, github)
            .await
            .unwrap();
        let paused = handle.service.get("jobs", &id).unwrap().unwrap();
        assert_eq!(paused["autoRecover"], false);
        handle.close().await.unwrap();
        drop(f.root);
    }
    #[tokio::test]
    async fn paused_unstarted_review_resumes_without_a_provider_session() {
        let f = Fixture::new().await;
        f.handle.admin("drain", &json!({})).await.unwrap();
        let args = json!({"repo":"owner/project","number":1});
        let queued = f.handle.admin("review", &args).await.unwrap();
        f.handle.admin("pause", &args).await.unwrap();
        let paused = f.job(s(&queued, "id"));
        assert_eq!(paused["state"], "paused");
        assert!(paused["session"].is_null());
        assert!(paused["startedAt"].is_null());
        let resumed = f.handle.admin("resume", &args).await.unwrap();
        assert_eq!(resumed["id"], queued["id"]);
        assert_eq!(resumed["state"], "queued");
        assert!(f.claim().await.is_null());
        f.handle.admin("undrain", &json!({})).await.unwrap();
        assert_eq!(f.claim().await["id"], queued["id"]);
        f.close().await;
    }
    #[tokio::test]
    async fn invalid_resume_overrides_leave_the_paused_review_unchanged() {
        let f = Fixture::new().await;
        let queued = f.queue().await;
        let args = json!({"repo":"owner/project","number":1});
        f.handle.admin("pause", &args).await.unwrap();
        let before = f.job(s(&queued, "id"));
        for field in ["model", "effort"] {
            let mut invalid = args.clone();
            invalid[field] = json!("");
            assert!(f.handle.admin("resume", &invalid).await.is_err());
            assert_eq!(f.job(s(&queued, "id")), before);
        }
        f.close().await;
    }
    #[tokio::test]
    async fn retry_requires_session_and_cooldown_never_shortens() {
        let f = Fixture::new().await;
        let j = f.prepared().await;
        f.worker(
            "failed",
            json!({"id":j["id"],"lease":j["lease"],"kind":"transient"}),
        )
        .await
        .unwrap();
        assert_eq!(f.job(s(&j, "id"))["state"], "paused");
        assert!(
            f.handle
                .admin("resume", &json!({"repo":"owner/project","number":1}))
                .await
                .is_err()
        );
        f.handle
            .admin("restart", &json!({"repo":"owner/project","number":1}))
            .await
            .unwrap();
        let j = f.claim().await;
        let deadline = now() + 900000;
        f.handle
            .service
            .db(|db| db.put("state", "cooldown", &json!(deadline)))
            .unwrap();
        f.worker("failed",json!({"id":j["id"],"lease":j["lease"],"kind":"transient","session":"12345678-abcd-abcd-abcd-123456789012"})).await.unwrap();
        assert_eq!(f.job(s(&j, "id"))["state"], "retrying");
        assert_eq!(
            f.handle.service.get("state", "cooldown").unwrap().unwrap(),
            deadline
        );
        f.close().await;
    }
    #[tokio::test]
    async fn shutdown_waits_for_startup_catch_up() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config::defaults(root.path());
        config["port"] = json!(0);
        let github = Arc::new(FakeGitHub::default());
        github.prs.lock().unwrap().push(pr(1));
        let db = Store::new(&root.path().join("service.sqlite")).unwrap();
        db.enroll(&json!({"name":"owner/project","installation":11,"worker":config["worker"]["id"],"policy":"everyone","authors":[],"requesters":[],"settings":{},"excluded":[],"enrolledAt":0})).unwrap();
        drop(db);
        let gate = Gate::new();
        *github.prs_gate.lock().unwrap() = Some(gate.clone());
        let handle = start_service_with_github(config, root.path().to_owned(), github)
            .await
            .unwrap();
        gate.wait().await;
        let closing = tokio::spawn(handle.close());
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        gate.open();
        closing.await.unwrap().unwrap();
        let db = Store::new(&root.path().join("service.sqlite")).unwrap();
        assert_eq!(db.all("jobs").unwrap().len(), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_wire_claims_share_one_executor_and_respect_capacity() {
        let f = Fixture::new().await;
        *f.github.prs.lock().unwrap() = (1..=10).map(pr).collect();
        assert_eq!(f.catch_up(false).await["queued"], 10);
        let mut requests = tokio::task::JoinSet::new();
        let client = reqwest::Client::new();
        for _ in 0..40 {
            let url = format!("http://{}/worker/next", f.handle.address());
            let token = s(&f.config["worker"], "token").to_owned();
            let client = client.clone();
            requests.spawn(async move {
                client
                    .post(url)
                    .bearer_auth(token)
                    .json(&json!({"active":[]}))
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            });
        }
        let mut ids = HashSet::new();
        while let Some(result) = requests.join_next().await {
            let result = result.unwrap();
            if !result.is_null() {
                assert!(ids.insert(s(&result["job"], "id").to_owned()));
            }
        }
        assert_eq!(ids.len(), 3);
        let threads = f.github.threads.lock().unwrap().clone();
        assert_eq!(threads.len(), 1);
        assert!(!threads.contains(&std::thread::current().id()));
        f.close().await;
    }
    #[tokio::test]
    async fn target_advance_rechecks_saved_report_without_repeating_analysis() {
        let f = Fixture::new().await;
        let j = f.prepared().await;
        f.report(&j).await;
        f.github.prs.lock().unwrap()[0]["base"]["sha"] = json!("d".repeat(40));
        f.publish_tick().await;
        assert_eq!(f.job(s(&j, "id"))["state"], "queued");
        assert!(f.github.published.lock().unwrap().is_empty());
        let current = f.claim().await;
        let mut c = comparison();
        c["targetSha"] = json!("d".repeat(40));
        f.worker(
            "comparison",
            json!({"id":current["id"],"lease":current["lease"],"comparison":c}),
        )
        .await
        .unwrap();
        assert!(!f.job(s(&j, "id"))["report"].is_null());
        f.report(&current).await;
        f.publish_tick().await;
        assert_eq!(f.job(s(&j, "id"))["state"], "completed");
        assert_eq!(f.github.published.lock().unwrap().len(), 1);
        f.close().await;
    }
    #[tokio::test]
    async fn completed_comparison_is_reconstructed_only_from_own_bot_marker() {
        for bot in [42, 99] {
            let f = Fixture::new().await;
            f.queue().await;
            let j = f.claim().await;
            let saved = patch(
                j.clone(),
                &json!({"comparison":comparison(),"settings":review_settings(&f.config["worker"]),"report":{"summary":"Earlier review","findings":[]}}),
            );
            let body = report::report_body(&saved, &[], &[]).unwrap();
            f.github.reviews.lock().unwrap().push(json!({"id":1,"body":body,"user":{"id":bot},"html_url":"https://github.com/owner/project/pull/1#pullrequestreview-1"}));
            let result = f
                .worker(
                    "comparison",
                    json!({"id":j["id"],"lease":j["lease"],"comparison":comparison()}),
                )
                .await
                .unwrap();
            assert_eq!(
                result.get("skip").and_then(Value::as_bool).unwrap_or(false),
                bot == 42
            );
            assert_eq!(
                f.job(s(&j, "id"))["state"],
                if bot == 42 { "completed" } else { "reviewing" }
            );
            f.close().await;
        }
    }
    #[tokio::test]
    async fn inclusive_scan_rechecks_policy_after_network_response() {
        let f = Fixture::new().await;
        f.handle
            .admin(
                "config-repo",
                &json!({"repo":"owner/project","requesters":["alice"]}),
            )
            .await
            .unwrap();
        let gate = Gate::new();
        *f.github.prs_gate.lock().unwrap() = Some(gate.clone());
        let service = f.handle.service.clone();
        let scan = tokio::spawn(
            f.handle
                .execute(async move { service.catch_up("owner/project", true).await }),
        );
        gate.wait().await;
        f.handle
            .admin(
                "config-repo",
                &json!({"repo":"owner/project","authors":["bob"]}),
            )
            .await
            .unwrap();
        gate.open();
        assert_eq!(scan.await.unwrap().unwrap(), json!({"queued":0,"held":0}));
        assert_eq!(
            f.handle.service.repo("owner/project").unwrap()["authors"],
            json!(["bob"])
        );
        f.close().await;
    }
    #[tokio::test]
    async fn enrollment_excludes_backlog_and_reenrollment_preserves_policy() {
        let f = Fixture::new().await;
        let enrolled = f
            .handle
            .admin(
                "enroll",
                &json!({"repo":"owner/other","githubToken":"operator-token","policy":"everyone"}),
            )
            .await
            .unwrap();
        assert_eq!(enrolled["excluded"], json!([1]));
        assert!(
            f.handle
                .admin(
                    "enroll",
                    &json!({"repo":"owner/other","githubToken":"operator-token"})
                )
                .await
                .is_err()
        );
        let updated=f.handle.admin("enroll",&json!({"repo":"owner/other","githubToken":"operator-token","reenroll":true,"policy":"selected","authors":["bob"]})).await.unwrap();
        assert_eq!(updated["policy"], "everyone");
        assert_eq!(updated["excluded"], json!([1]));
        f.close().await;
    }
    #[tokio::test]
    async fn slow_request_body_does_not_block_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let f = Fixture::new().await;
        let mut socket = tokio::net::TcpStream::connect(f.handle.address())
            .await
            .unwrap();
        let request = format!(
            "POST /admin/drain HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 1000\r\n\r\n{{",
            s(&f.config, "adminToken")
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::time::timeout(Duration::from_secs(2), f.handle.close())
            .await
            .unwrap()
            .unwrap();
        let mut output = String::new();
        socket.read_to_string(&mut output).await.unwrap();
        assert!(output.contains("503"));
        drop(f.root);
    }
    #[tokio::test]
    async fn deferred_events_preserve_repository_order_without_blocking_others() {
        let f = Fixture::new().await;
        f.github.prs.lock().unwrap().push(pr(2));
        let mut second = f.handle.service.repo("owner/project").unwrap();
        second["name"] = json!("owner/second");
        f.handle
            .service
            .db(|db| db.enroll(&second).map(|_| ()))
            .unwrap();
        for (id, repo, number) in [
            ("first", "owner/project", 1),
            ("second", "owner/project", 1),
            ("other", "owner/second", 2),
        ] {
            f.handle.service.db(|db|db.accept_event(id,&json!({"type":"pull_request","repo":repo,"number":number,"action":"opened"}))).unwrap();
        }
        f.github.fail_pr.store(true, Ordering::Relaxed);
        let service = f.handle.service.clone();
        f.handle
            .execute(async move { service.tick().await })
            .await
            .unwrap();
        let events = f.handle.service.db(|db| db.events()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["id"], "first");
        assert_eq!(events[1]["id"], "second");
        assert!(n(&events[0], "nextAt") > now());
        let jobs = f.handle.service.all("jobs").unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["repo"], "owner/second");
        f.handle
            .service
            .db(|db| db.defer_event(&events[0], 0))
            .unwrap();
        let service = f.handle.service.clone();
        f.handle
            .execute(async move { service.tick().await })
            .await
            .unwrap();
        assert!(f.handle.service.db(|db| db.events()).unwrap().is_empty());
        assert_eq!(f.handle.service.all("jobs").unwrap().len(), 2);
        f.close().await;
    }
}
