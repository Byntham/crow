//! Durable, bounded delegation. Child reviews cannot delegate further.
use crate::provider::{Callbacks, classify_error};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};
use tokio_util::sync::CancellationToken;

type RunFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
type Runner = Arc<
    dyn Fn(Value, Value, Value, PathBuf, Callbacks, CancellationToken) -> RunFuture + Send + Sync,
>;
#[derive(Clone)]
pub struct Delegation {
    inner: Arc<Inner>,
}
struct Inner {
    context: Value,
    dir: PathBuf,
    epoch: u64,
    state: Mutex<State>,
    calls: AsyncMutex<()>,
    persistence: AsyncMutex<()>,
    slots: Arc<Semaphore>,
    run: Runner,
}
#[derive(Default)]
struct State {
    tasks: BTreeMap<String, Value>,
    active: BTreeMap<String, Active>,
    stopping: bool,
}
struct Active {
    cancel: CancellationToken,
    done: watch::Receiver<bool>,
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
fn task_id() -> String {
    use rand::RngCore;
    let mut bytes = [0; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
fn remove(value: &mut Value, fields: &[&str]) {
    if let Some(obj) = value.as_object_mut() {
        for key in fields {
            obj.remove(*key);
        }
    }
}
fn selection(settings: &Value) -> Value {
    json!({"model":settings["model"],"effort":settings["effort"]})
}
fn effective(parent: &Value) -> Value {
    if parent["subagents"]["mode"] == "configured" {
        selection(&parent["subagents"])
    } else {
        selection(parent)
    }
}
fn valid_selection(v: &Value) -> bool {
    v.is_object()
        && ["model", "effort"]
            .iter()
            .all(|k| v.get(*k).is_some_and(|x| x.is_null() || x.is_string()))
}
fn valid_saved(v: &Value) -> bool {
    let s = &v["settings"];
    v.is_object()
        && ["id", "task", "createdAt"]
            .iter()
            .all(|k| v[*k].is_string())
        && v["id"].as_str().is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        && matches!(
            v["state"].as_str(),
            Some("queued" | "running" | "paused" | "completed" | "superseded")
        )
        && valid_selection(s)
        && s["timeoutMs"].is_number()
        && s["subagents"]["mode"] == "inherit"
        && s["subagents"]["max"].is_number()
        && matches!(s["retry"]["mode"].as_str(), Some("fixed" | "progressive"))
        && s["retry"]["count"].is_number()
        && s["retry"]["delayMs"].is_number()
        && ["codex", "codexHome"]
            .iter()
            .all(|k| s.get(*k).is_none_or(Value::is_string))
        && [
            "session",
            "error",
            "errorKind",
            "diagnostic",
            "replaces",
            "replacement",
            "updatedAt",
        ]
        .iter()
        .all(|k| v.get(*k).is_none_or(Value::is_string))
        && [
            "operatorResumeEpoch",
            "consecutiveFailures",
            "outputFailures",
            "nextAttemptAt",
        ]
        .iter()
        .all(|k| v.get(*k).is_none_or(Value::is_number))
        && v.get("requiresOperator").is_none_or(Value::is_boolean)
        && v.get("settingsHistory").is_none_or(|h| {
            h.as_array().is_some_and(|a| {
                a.iter().all(|c| {
                    c["resumeEpoch"].is_number()
                        && c["changedAt"].is_string()
                        && valid_selection(&c["previous"])
                        && valid_selection(&c["next"])
                })
            })
        })
        && v.get("report").is_none_or(|r| {
            r["summary"].is_string()
                && r["findings"].as_array().is_some_and(|a| {
                    a.iter().all(|f| {
                        ["id", "title", "body", "path"]
                            .iter()
                            .all(|k| f[*k].is_string())
                            && f["line"].is_number()
                            && matches!(
                                f["severity"].as_str(),
                                Some("critical" | "high" | "medium" | "low")
                            )
                    })
                })
        })
}
async fn atomic(path: &Path, value: &Value) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let temp = path.with_extension(format!("{}.tmp", task_id()));
    let result: Result<()> = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        file.write_all(&serde_json::to_vec(value)?).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, path).await?;
        #[cfg(unix)]
        tokio::fs::File::open(path.parent().expect("Task directory exists"))
            .await?
            .sync_all()
            .await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}
pub fn tools() -> Value {
    Value::Array(delegation_tools())
}
pub fn delegation_tools() -> Vec<Value> {
    vec![
        json!({"name":"start_review_task","description":"Delegate a bounded independent code-inspection task. Model, reasoning, permissions and concurrency are fixed by Crow. Await relevant results before returning the consolidated report.","properties":{"task":{"type":"string"}},"required":["task"]}),
        json!({"name":"review_task_status","description":"Read saved delegated task states and complete reports. Paused tasks need explicit resume.","properties":{"id":{"type":"string"}}}),
        json!({"name":"resume_review_task","description":"Resume a paused task from its saved session after nextAttemptAt. Retry limits are enforced by Crow.","properties":{"id":{"type":"string"}},"required":["id"]}),
        json!({"name":"restart_review_task","description":"Replace a paused task without a saved session. Retains its task, settings and failure budget.","properties":{"id":{"type":"string"}},"required":["id"]}),
        json!({"name":"wait_review_task","description":"Wait briefly for a delegated task and return its state and complete report when available.","properties":{"id":{"type":"string"},"timeoutMs":{"type":"integer","minimum":1,"maximum":30000}},"required":["id"]}),
    ]
}
impl Delegation {
    pub async fn init(context: Value) -> Result<Self> {
        let runner: Runner = Arc::new(|job, source, guidance, root, callbacks, cancel| {
            Box::pin(async move {
                crate::provider::run_review(&job, &source, &guidance, &root, callbacks, cancel)
                    .await
            })
        });
        Self::init_with_runner(context, runner).await
    }
    async fn init_with_runner(context: Value, run: Runner) -> Result<Self> {
        let root = context["root"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing delegation root"))?;
        let job_id = context["job"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing review ID"))?;
        ensure!(
            !job_id.is_empty()
                && job_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "Invalid review ID"
        );
        let dir = Path::new(root).join("reviews").join(job_id).join("tasks");
        tokio::fs::create_dir_all(&dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let epoch = context["job"]["resumeEpoch"].as_u64().unwrap_or(0);
        let max = context["job"]["settings"]["subagents"]["max"]
            .as_u64()
            .unwrap_or(8);
        ensure!(
            max <= Semaphore::MAX_PERMITS as u64,
            "Invalid delegated-task concurrency limit"
        );
        let max = max as usize;
        let this = Self {
            inner: Arc::new(Inner {
                context,
                dir,
                epoch,
                state: Mutex::new(State::default()),
                calls: AsyncMutex::new(()),
                persistence: AsyncMutex::new(()),
                slots: Arc::new(Semaphore::new(max)),
                run,
            }),
        };
        let mut entries = tokio::fs::read_dir(&this.inner.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                continue;
            }
            let mut task: Value = serde_json::from_slice(&tokio::fs::read(entry.path()).await?)?;
            ensure!(
                valid_saved(&task) && task["id"] == id,
                "Invalid saved delegated task: {name}"
            );
            let mut changed = false;
            if task["state"] == "running" {
                task["state"] = json!("paused");
                task["errorKind"] = json!("interrupted");
                changed = true;
            }
            if epoch > task["operatorResumeEpoch"].as_u64().unwrap_or(0) {
                task["operatorResumeEpoch"] = json!(epoch);
                changed = true;
                if matches!(task["state"].as_str(), Some("paused" | "queued")) {
                    task["requiresOperator"] = json!(false);
                    task["consecutiveFailures"] = json!(0);
                    task["outputFailures"] = json!(0);
                    remove(&mut task, &["nextAttemptAt"]);
                    let next = effective(&this.inner.context["job"]["settings"]);
                    let previous = selection(&task["settings"]);
                    if next != previous {
                        if task.get("settingsHistory").is_none() {
                            task["settingsHistory"] = json!([]);
                        }
                        task["settingsHistory"].as_array_mut().unwrap().push(json!({"resumeEpoch":epoch,"changedAt":now(),"previous":previous,"next":next}));
                        task["settings"]["model"] = next["model"].clone();
                        task["settings"]["effort"] = next["effort"].clone();
                    }
                }
            }
            if changed {
                task["updatedAt"] = json!(now());
                atomic(&entry.path(), &task).await?;
            }
            this.inner
                .state
                .lock()
                .unwrap()
                .tasks
                .insert(id.to_owned(), task);
        }
        Ok(this)
    }
    async fn update(&self, id: &str, change: impl FnOnce(&mut Value)) -> Result<()> {
        let _order = self.inner.persistence.lock().await;
        let snapshot = {
            let mut state = self.inner.state.lock().unwrap();
            let task = state
                .tasks
                .get_mut(id)
                .ok_or_else(|| anyhow!("Unknown delegated task ID"))?;
            change(task);
            task["updatedAt"] = json!(now());
            task.clone()
        };
        atomic(&self.inner.dir.join(format!("{id}.json")), &snapshot).await
    }
    fn task(&self, id: &str) -> Result<Value> {
        self.inner
            .state
            .lock()
            .unwrap()
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown delegated task ID"))
    }
    fn view(&self, id: &str) -> Result<Value> {
        let state = self.inner.state.lock().unwrap();
        let t = state
            .tasks
            .get(id)
            .ok_or_else(|| anyhow!("Unknown delegated task ID"))?;
        let active = state.active.contains_key(id);
        let operator = t["requiresOperator"].as_bool().unwrap_or(false);
        let has_session = t["session"].as_str().is_some_and(|s| !s.is_empty());
        let mut view = json!({"id":id,"state":if active {json!("running")} else {t["state"].clone()},"task":t["task"],"model":t["settings"]["model"],"effort":t["settings"]["effort"],"requiresOperator":!active && operator,"canResume":!active && t["state"] == "paused" && has_session && !operator,"canRestart":!active && t["state"] == "paused" && !has_session && !operator});
        if let Some(v) = t.get("replacement") {
            view["replacement"] = v.clone();
        }
        if !active {
            for field in ["report", "nextAttemptAt"] {
                if let Some(v) = t.get(field) {
                    view[field] = v.clone();
                }
            }
            if t.get("errorKind").is_some() || t.get("error").is_some() {
                view["errorKind"] = t.get("errorKind").cloned().unwrap_or(json!("transient"));
                view["error"] = json!(if operator {
                    "Delegated review paused. Operator action is required before further attempts."
                } else if t["errorKind"] == "interrupted" {
                    "Delegated review interrupted. Resume its saved session when available."
                } else if t["errorKind"] == "output" {
                    "Delegated review returned an invalid final report. Resume to correct it after nextAttemptAt."
                } else if has_session {
                    "Delegated review failed. Resume its saved session after nextAttemptAt."
                } else {
                    "Delegated review failed before saving a session. Use restart_review_task after nextAttemptAt."
                });
            }
        }
        Ok(view)
    }
    async fn check_retry(&self, id: &str) -> Result<()> {
        let task = self.task(id)?;
        if task["requiresOperator"] == true
            || task["consecutiveFailures"].as_u64().unwrap_or(0)
                > task["settings"]["retry"]["count"].as_u64().unwrap_or(10)
            || task["outputFailures"].as_u64().unwrap_or(0) > 2
        {
            self.update(id, |t| t["requiresOperator"] = json!(true))
                .await?;
            bail!(
                "Delegated task requires operator action; automatic attempts are exhausted or unavailable."
            );
        }
        ensure!(
            task["nextAttemptAt"].as_u64().unwrap_or(0) <= millis(),
            "Delegated task retry is delayed until {}.",
            task["nextAttemptAt"]
        );
        Ok(())
    }
    async fn launch(&self, id: &str, predecessor: Option<&str>) -> Result<Value> {
        let max = self.inner.context["job"]["settings"]["subagents"]["max"]
            .as_u64()
            .unwrap_or(8);
        let permit = self.inner.slots.clone().try_acquire_owned().map_err(|_| {
            anyhow!(
                "The {max} concurrent delegated-task limit is reached. Wait for a task to finish."
            )
        })?;
        let cancel = CancellationToken::new();
        let (done_tx, done_rx) = watch::channel(false);
        {
            let mut state = self.inner.state.lock().unwrap();
            ensure!(!state.stopping, "Review is stopping");
            ensure!(
                !state.active.contains_key(id),
                "Delegated task is still running or saving its result"
            );
            state.active.insert(
                id.to_owned(),
                Active {
                    cancel: cancel.clone(),
                    done: done_rx,
                },
            );
        }
        let starting = async {
            self.update(id, |t| {
                t["state"] = json!("running");
                remove(t, &["error", "errorKind", "nextAttemptAt"]);
            })
            .await?;
            if let Some(old) = predecessor {
                self.update(old, |_| {}).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = starting {
            self.inner.state.lock().unwrap().active.remove(id);
            return Err(e);
        }
        let this = self.clone();
        let id = id.to_owned();
        let view = self.view(&id)?;
        tokio::spawn(async move {
            let task = this.task(&id).expect("Reserved delegated task exists");
            let mut child = this.inner.context["job"].clone();
            child["id"] = json!(format!(
                "{}_{}",
                child["id"].as_str().unwrap_or("review"),
                id
            ));
            child["parentId"] = this.inner.context["job"]["id"].clone();
            child["taskId"] = json!(id);
            child["task"] = task["task"].clone();
            child["session"] = task["session"].clone();
            child["delegated"] = json!(true);
            child["settings"] = task["settings"].clone();
            child["settings"]["codexProxy"] =
                this.inner.context["job"]["settings"]["codexProxy"].clone();
            child["settings"]["detached"] = json!(false);
            let session_this = this.clone();
            let session_id = id.clone();
            let progress_this = this.clone();
            let progress_id = id.clone();
            let callbacks = Callbacks {
                on_session: Some(Arc::new(move |session| {
                    let this = session_this.clone();
                    let id = session_id.clone();
                    Box::pin(async move {
                        ensure!(session.is_string(), "Invalid delegated session");
                        this.update(&id, |t| t["session"] = session).await
                    })
                })),
                on_progress: Some(Arc::new(move |event| {
                    let this = progress_this.clone();
                    let id = progress_id.clone();
                    Box::pin(async move {
                        if matches!(
                            event["type"].as_str(),
                            Some("mcp_tool_call" | "agent_message" | "reasoning")
                        ) {
                            this.update(&id, |t| t["consecutiveFailures"] = json!(0))
                                .await?;
                        }
                        Ok(())
                    })
                })),
            };
            let result = if cancel.is_cancelled() {
                Err(anyhow!("Parent review stopped"))
            } else {
                (this.inner.run)(
                    child,
                    this.inner.context["source"].clone(),
                    this.inner.context["guidance"].clone(),
                    PathBuf::from(this.inner.context["root"].as_str().unwrap()),
                    callbacks,
                    cancel.clone(),
                )
                .await
            };
            let final_save = this
                .update(&id, |task| match result {
                    Ok(report) => {
                        task["report"] = report;
                        task["state"] = json!("completed");
                        task["consecutiveFailures"] = json!(0);
                        task["outputFailures"] = json!(0);
                        task["requiresOperator"] = json!(false);
                    }
                    Err(error) => {
                        task["state"] = json!("paused");
                        remove(task, &["report"]);
                        if cancel.is_cancelled() {
                            task["errorKind"] = json!("interrupted");
                            return;
                        }
                        let failure = classify_error(&error);
                        let kind = if ["auth", "quota", "config", "restart", "output", "transient"]
                            .contains(&failure.kind.as_str())
                        {
                            failure.kind.as_str()
                        } else {
                            "transient"
                        };
                        let failures = task["consecutiveFailures"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_add(1);
                        let output = task["outputFailures"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_add(u64::from(kind == "output"));
                        task["errorKind"] = json!(kind);
                        task["diagnostic"] = json!(failure.message);
                        task["consecutiveFailures"] = json!(failures);
                        task["outputFailures"] = json!(output);
                        task["requiresOperator"] = json!(
                            ["auth", "quota", "config", "restart"].contains(&kind)
                                || failures
                                    > task["settings"]["retry"]["count"].as_u64().unwrap_or(10)
                                || output > 2
                        );
                        let schedule = [5000, 15000, 30000, 60000, 120000, 300000];
                        let delay = if task["settings"]["retry"]["mode"] == "progressive" {
                            schedule[failures.saturating_sub(1).min(5) as usize]
                        } else {
                            task["settings"]["retry"]["delayMs"]
                                .as_u64()
                                .unwrap_or(5000)
                        };
                        task["nextAttemptAt"] =
                            json!(millis().saturating_add(delay.max(failure.retry_after)));
                    }
                })
                .await;
            // A failed final write must not expose an undurable completed result.
            if let Err(error) = final_save {
                let mut state = this.inner.state.lock().unwrap();
                if let Some(t) = state.tasks.get_mut(&id) {
                    t["state"] = json!("paused");
                    t["errorKind"] = json!("transient");
                    t["diagnostic"] = json!(error.to_string());
                    remove(t, &["report"]);
                }
            }
            this.inner.state.lock().unwrap().active.remove(&id);
            drop(permit);
            let _ = done_tx.send(true);
        });
        Ok(view)
    }
    pub async fn call(&self, name: &str, input: &Value) -> Result<Value> {
        let definitions = delegation_tools();
        let definition = definitions
            .iter()
            .find(|d| d["name"] == name)
            .ok_or_else(|| anyhow!("Unknown delegation tool"))?;
        let args = input.as_object().ok_or_else(|| {
            anyhow!(
                "Unsupported delegated-task arguments; model and reasoning are controlled by Crow"
            )
        })?;
        ensure!(
            args.keys()
                .all(|k| definition["properties"].get(k).is_some()),
            "Unsupported delegated-task arguments; model and reasoning are controlled by Crow"
        );
        if name == "wait_review_task" {
            let id = input["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Unknown delegated task ID"))?;
            self.task(id)?;
            let ms = match args.get("timeoutMs") {
                None => 1000,
                Some(v) => v
                    .as_u64()
                    .ok_or_else(|| anyhow!("Wait must be between 1 and 30000 milliseconds"))?,
            };
            ensure!(
                (1..=30000).contains(&ms),
                "Wait must be between 1 and 30000 milliseconds"
            );
            let pending = self
                .inner
                .state
                .lock()
                .unwrap()
                .active
                .get(id)
                .map(|a| a.done.clone());
            if let Some(mut done) = pending {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(ms), async {
                    if !*done.borrow() {
                        let _ = done.changed().await;
                    }
                })
                .await;
            }
            return self.view(id);
        }
        let _calls = self.inner.calls.lock().await;
        if name == "review_task_status" {
            return match input.get("id") {
                None | Some(Value::Null) => {
                    let ids: Vec<_> = self
                        .inner
                        .state
                        .lock()
                        .unwrap()
                        .tasks
                        .keys()
                        .cloned()
                        .collect();
                    Ok(Value::Array(
                        ids.iter()
                            .map(|id| self.view(id))
                            .collect::<Result<Vec<_>>>()?,
                    ))
                }
                Some(v) => self.view(
                    v.as_str()
                        .ok_or_else(|| anyhow!("Unknown delegated task ID"))?,
                ),
            };
        }
        if name == "start_review_task" {
            ensure!(
                self.inner.context["job"]["delegated"] != true,
                "Delegated reviews cannot delegate further"
            );
            let text = input["task"]
                .as_str()
                .ok_or_else(|| anyhow!("A delegated task needs 1–16000 characters"))?;
            ensure!(
                !text.trim().is_empty() && text.encode_utf16().count() <= 16000,
                "A delegated task needs 1–16000 characters"
            );
            let parent = &self.inner.context["job"]["settings"];
            let selected = effective(parent);
            let mut settings = json!({"model":selected["model"],"effort":selected["effort"],"timeoutMs":parent["timeoutMs"].as_u64().unwrap_or(0),"retry":parent.get("retry").cloned().unwrap_or(json!({"mode":"fixed","count":10,"delayMs":5000})),"subagents":{"mode":"inherit","max":0}});
            for field in ["codex", "codexHome"] {
                if let Some(v) = parent.get(field).filter(|v| v.is_string()) {
                    settings[field] = v.clone();
                }
            }
            let id = task_id();
            let task = json!({"id":id,"task":text,"settings":settings,"state":"queued","createdAt":now(),"operatorResumeEpoch":self.inner.epoch});
            self.inner
                .state
                .lock()
                .unwrap()
                .tasks
                .insert(id.clone(), task);
            let result = self.launch(&id, None).await;
            if result.is_err() {
                self.inner.state.lock().unwrap().tasks.remove(&id);
            }
            return result;
        }
        let id = input["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Unknown delegated task ID"))?;
        let task = self.task(id)?;
        ensure!(
            !self.inner.state.lock().unwrap().active.contains_key(id) && task["state"] == "paused",
            "Only a paused delegated task can resume or restart"
        );
        let has_session = task["session"].as_str().is_some_and(|s| !s.is_empty());
        if name == "resume_review_task" {
            ensure!(
                has_session,
                "No usable saved session; use restart_review_task"
            );
            self.check_retry(id).await?;
            return self.launch(id, None).await;
        }
        ensure!(
            !has_session,
            "Only a paused delegated task without a saved session can restart"
        );
        self.check_retry(id).await?;
        let replacement = task_id();
        let next = json!({"id":replacement,"task":task["task"],"settings":task["settings"],"state":"queued","createdAt":now(),"consecutiveFailures":task["consecutiveFailures"].as_u64().unwrap_or(0),"outputFailures":task["outputFailures"].as_u64().unwrap_or(0),"replaces":id,"operatorResumeEpoch":self.inner.epoch});
        {
            let mut state = self.inner.state.lock().unwrap();
            let old = state.tasks.get_mut(id).unwrap();
            old["state"] = json!("superseded");
            old["replacement"] = json!(replacement);
            state.tasks.insert(replacement.clone(), next);
        }
        let result = self.launch(&replacement, Some(id)).await;
        if result.is_err() {
            self.inner.state.lock().unwrap().tasks.remove(&replacement);
            self.update(id, |t| {
                t["state"] = json!("paused");
                remove(t, &["replacement"]);
            })
            .await?;
            let _ =
                tokio::fs::remove_file(self.inner.dir.join(format!("{replacement}.json"))).await;
        }
        result
    }
    pub async fn close(&self) {
        let active = {
            let _calls = self.inner.calls.lock().await;
            let mut state = self.inner.state.lock().unwrap();
            state.stopping = true;
            state
                .active
                .values()
                .map(|a| {
                    a.cancel.cancel();
                    a.done.clone()
                })
                .collect::<Vec<_>>()
        };
        for mut done in active {
            if !*done.borrow() {
                let _ = done.changed().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(root: &Path, max: u64) -> Value {
        json!({"root":root,"source":{},"guidance":{},"job":{"id":"review1","settings":{"model":"model-a","effort":"high","timeoutMs":0,"retry":{"mode":"fixed","count":1,"delayMs":0},"subagents":{"mode":"inherit","max":max}}}})
    }
    fn waiting_runner() -> Runner {
        Arc::new(|_, _, _, _, callbacks, cancel| {
            Box::pin(async move {
                if let Some(cb) = callbacks.on_session {
                    cb(json!("session1")).await?;
                }
                cancel.cancelled().await;
                Err(anyhow!("interrupted"))
            })
        })
    }
    async fn complete(d: &Delegation, id: &str) -> Value {
        d.call("wait_review_task", &json!({"id":id,"timeoutMs":30000}))
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn concurrency_is_reserved_before_launch_and_close_saves_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Delegation::init_with_runner(context(tmp.path(), 1), waiting_runner())
            .await
            .unwrap();
        let first = json!({"task":"first"});
        let second = json!({"task":"second"});
        let (a, b) = tokio::join!(
            d.call("start_review_task", &first),
            d.call("start_review_task", &second)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let task = a.or(b).unwrap();
        let id = task["id"].as_str().unwrap();
        for _ in 0..100 {
            if d.task(id).unwrap()["session"] == "session1" {
                break;
            }
            tokio::task::yield_now().await;
        }
        d.close().await;
        let status = d.view(id).unwrap();
        assert_eq!(status["state"], "paused");
        assert_eq!(status["errorKind"], "interrupted");
        assert_eq!(status["canResume"], true);
        let recovered = Delegation::init_with_runner(context(tmp.path(), 1), waiting_runner())
            .await
            .unwrap();
        assert_eq!(recovered.view(id).unwrap()["canResume"], true);
        assert!(
            d.call("start_review_task", &json!({"task":"after close"}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn child_settings_and_reports_are_preserved_without_nested_delegation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = context(tmp.path(), 2);
        ctx["job"]["settings"]["subagents"] =
            json!({"mode":"configured","max":2,"model":"child-model","effort":"low"});
        ctx["job"]["settings"]["codexProxy"] = json!({"url":"http://proxy"});
        let run: Runner = Arc::new(|job, _, _, _, _, _| {
            Box::pin(async move {
                assert_eq!(job["settings"]["model"], "child-model");
                assert_eq!(job["settings"]["subagents"]["max"], 0);
                assert_eq!(job["settings"]["detached"], false);
                assert_eq!(job["settings"]["codexProxy"]["url"], "http://proxy");
                assert_eq!(job["task"], "inspect auth");
                Ok(json!({"summary":"done","findings":[]}))
            })
        });
        let d = Delegation::init_with_runner(ctx, run).await.unwrap();
        let launched = d
            .call("start_review_task", &json!({"task":"inspect auth"}))
            .await
            .unwrap();
        let status = complete(&d, launched["id"].as_str().unwrap()).await;
        assert_eq!(status["state"], "completed");
        assert_eq!(status["report"]["summary"], "done");
        assert!(
            d.call(
                "start_review_task",
                &json!({"task":"inspect","model":"override"})
            )
            .await
            .is_err()
        );
        let mut nested = context(tmp.path(), 2);
        nested["job"]["delegated"] = json!(true);
        let child = Delegation::init_with_runner(nested, waiting_runner())
            .await
            .unwrap();
        assert!(
            child
                .call("start_review_task", &json!({"task":"nested"}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn failure_details_are_redacted_and_replacement_keeps_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Runner = Arc::new(|_, _, _, _, _, _| {
            Box::pin(async { Err(anyhow!("private backend diagnostic 123")) })
        });
        let d = Delegation::init_with_runner(context(tmp.path(), 2), runner)
            .await
            .unwrap();
        let first = d
            .call("start_review_task", &json!({"task":"inspect"}))
            .await
            .unwrap();
        let id = first["id"].as_str().unwrap();
        let paused = complete(&d, id).await;
        assert_eq!(paused["canRestart"], true);
        assert!(!paused.to_string().contains("private backend"));
        let second = d
            .call("restart_review_task", &json!({"id":id}))
            .await
            .unwrap();
        let next_id = second["id"].as_str().unwrap();
        let paused = complete(&d, next_id).await;
        assert_eq!(paused["requiresOperator"], true);
        assert_eq!(d.view(id).unwrap()["replacement"], next_id);
        assert_eq!(d.task(next_id).unwrap()["consecutiveFailures"], 2);
        assert!(
            d.call("restart_review_task", &json!({"id":next_id}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn epoch_resets_budget_and_changes_settings_once() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = context(tmp.path(), 1);
        let d = Delegation::init_with_runner(ctx.clone(), waiting_runner())
            .await
            .unwrap();
        let saved = json!({"id":"ab12","task":"inspect","createdAt":now(),"state":"running","settings":{"model":"old","effort":"low","timeoutMs":0,"subagents":{"mode":"inherit","max":0},"retry":{"mode":"fixed","count":1,"delayMs":0}},"requiresOperator":true,"consecutiveFailures":9,"outputFailures":3,"session":"saved","nextAttemptAt":u64::MAX});
        atomic(&d.inner.dir.join("ab12.json"), &saved)
            .await
            .unwrap();
        let mut resumed = ctx;
        resumed["job"]["resumeEpoch"] = json!(2);
        let d = Delegation::init_with_runner(resumed.clone(), waiting_runner())
            .await
            .unwrap();
        let task = d.task("ab12").unwrap();
        assert_eq!(task["state"], "paused");
        assert_eq!(task["consecutiveFailures"], 0);
        assert_eq!(task["settings"]["model"], "model-a");
        assert_eq!(task["settingsHistory"].as_array().unwrap().len(), 1);
        assert!(task.get("nextAttemptAt").is_none());
        d.update("ab12", |t| t["consecutiveFailures"] = json!(1))
            .await
            .unwrap();
        let d = Delegation::init_with_runner(resumed, waiting_runner())
            .await
            .unwrap();
        assert_eq!(d.task("ab12").unwrap()["consecutiveFailures"], 1);
        assert_eq!(
            d.task("ab12").unwrap()["settingsHistory"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[tokio::test]
    async fn saved_ids_cannot_escape_the_task_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Delegation::init_with_runner(context(tmp.path(), 1), waiting_runner())
            .await
            .unwrap();
        atomic(
            &d.inner.dir.join("ab12.json"),
            &json!({"id":"../escape","task":"x"}),
        )
        .await
        .unwrap();
        assert!(
            Delegation::init_with_runner(context(tmp.path(), 1), waiting_runner())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn unicode_task_budget_counts_utf16_units() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Delegation::init_with_runner(context(tmp.path(), 1), waiting_runner())
            .await
            .unwrap();
        assert!(
            d.call("start_review_task", &json!({"task":"😀".repeat(8001)}))
                .await
                .is_err()
        );
        assert!(
            d.call("wait_review_task", &json!({"id":"missing","timeoutMs":0}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn final_persistence_retains_the_concurrency_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let child_release = release.clone();
        let runner: Runner = Arc::new(move |_, _, _, _, _, _| {
            let release = child_release.clone();
            Box::pin(async move {
                release.notified().await;
                Ok(json!({"summary":"done","findings":[]}))
            })
        });
        let d = Delegation::init_with_runner(context(tmp.path(), 1), runner)
            .await
            .unwrap();
        let first = d
            .call("start_review_task", &json!({"task":"first"}))
            .await
            .unwrap();
        let id = first["id"].as_str().unwrap();
        let writing = d.inner.persistence.lock().await;
        release.notify_one();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(d.view(id).unwrap()["state"], "running");
        assert!(d.view(id).unwrap().get("report").is_none());
        assert!(
            d.call("start_review_task", &json!({"task":"over capacity"}))
                .await
                .is_err()
        );
        drop(writing);
        assert_eq!(complete(&d, id).await["state"], "completed");
    }
    #[tokio::test]
    async fn output_failure_limit_survives_progress_and_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Runner = Arc::new(|_, _, _, _, callbacks, _| {
            Box::pin(async move {
                callbacks.on_session.unwrap()(json!("saved-session")).await?;
                callbacks.on_progress.unwrap()(json!({"type":"reasoning"})).await?;
                Err(crate::provider::ProviderError {
                    kind: "output".into(),
                    message: "invalid final report".into(),
                    retry_after: 0,
                }
                .into())
            })
        });
        let d = Delegation::init_with_runner(context(tmp.path(), 1), runner)
            .await
            .unwrap();
        let first = d
            .call("start_review_task", &json!({"task":"inspect"}))
            .await
            .unwrap();
        let id = first["id"].as_str().unwrap();
        for attempt in 1..=3 {
            let status = complete(&d, id).await;
            assert_eq!(d.task(id).unwrap()["consecutiveFailures"], 1);
            assert_eq!(d.task(id).unwrap()["outputFailures"], attempt);
            assert_eq!(status["requiresOperator"], attempt == 3);
            if attempt < 3 {
                d.call("resume_review_task", &json!({"id":id}))
                    .await
                    .unwrap();
            }
        }
        assert!(
            d.call("resume_review_task", &json!({"id":id}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn provider_retry_delay_is_a_minimum() {
        let tmp = tempfile::tempdir().unwrap();
        let runner: Runner = Arc::new(|_, _, _, _, _, _| {
            Box::pin(async {
                Err(crate::provider::ProviderError {
                    kind: "transient".into(),
                    message: "busy".into(),
                    retry_after: 45000,
                }
                .into())
            })
        });
        let d = Delegation::init_with_runner(context(tmp.path(), 1), runner)
            .await
            .unwrap();
        let started = millis();
        let first = d
            .call("start_review_task", &json!({"task":"inspect"}))
            .await
            .unwrap();
        let id = first["id"].as_str().unwrap();
        let status = complete(&d, id).await;
        assert!(status["nextAttemptAt"].as_u64().unwrap() >= started + 45000);
        assert!(
            d.call("restart_review_task", &json!({"id":id}))
                .await
                .unwrap_err()
                .to_string()
                .contains("delayed")
        );
    }
}
