use crate::util::{id, now, private_dir, repo_name};
use anyhow::{Context, Result, bail};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn terminal(state: &Value) -> bool {
    matches!(
        state.as_str(),
        Some("completed" | "superseded" | "cancelled")
    )
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing or invalid {key}"))
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        _ => true,
    }
}
/// Stores compatible JSON records in the original SQLite schema.
pub struct Store {
    pub db: Connection,
}
impl Store {
    pub fn new(file: &Path) -> Result<Self> {
        if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
            private_dir(parent)?;
        }
        let db = Connection::open(file)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,id));
            CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, received INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, value TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending');
            CREATE INDEX IF NOT EXISTS jobs_key ON records(json_extract(value,'$.key')) WHERE kind='jobs';
            CREATE INDEX IF NOT EXISTS jobs_worker_state ON records(json_extract(value,'$.worker'),json_extract(value,'$.state')) WHERE kind='jobs';")?;
        Ok(Self { db })
    }
    pub fn tx<T>(&self, callback: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        // RAII also rolls back if a callback unwinds.
        let transaction =
            rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let value = callback(self)?;
        transaction.commit()?;
        Ok(value)
    }
    pub fn get(&self, kind: &str, key: &str) -> Result<Option<Value>> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM records WHERE kind=? AND id=?",
                params![kind, key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn all(&self, kind: &str) -> Result<Vec<Value>> {
        let mut statement = self
            .db
            .prepare("SELECT value FROM records WHERE kind=? ORDER BY rowid")?;
        statement
            .query_map([kind], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }
    pub fn put(&self, kind: &str, key: &str, value: &Value) -> Result<()> {
        self.db.execute("INSERT INTO records VALUES(?,?,?) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",params![kind,key,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn delete(&self, kind: &str, key: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM records WHERE kind=? AND id=?",
            params![kind, key],
        )?;
        Ok(())
    }
    pub fn accept_event(&self, delivery: &str, event: &Value) -> Result<bool> {
        self.tx(|store| {
            let inserted = store.db.execute(
                "INSERT OR IGNORE INTO receipts VALUES(?,?)",
                params![delivery, now()],
            )?;
            if inserted == 0 {
                return Ok(false);
            }
            store.db.execute(
                "INSERT INTO events(id,value) VALUES(?,?)",
                params![delivery, serde_json::to_string(event)?],
            )?;
            Ok(true)
        })
    }
    pub fn events(&self) -> Result<Vec<Value>> {
        let mut statement = self
            .db
            .prepare("SELECT id,value FROM events WHERE state='pending' ORDER BY rowid")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (key, encoded) = row?;
                let mut event: Value = serde_json::from_str(&encoded)?;
                let object = event.as_object_mut().context("Invalid persisted event")?;
                // The SQLite identity wins over payload data, preventing one event from deleting another.
                object.insert("id".into(), Value::String(key));
                Ok(event)
            })
            .collect()
    }
    pub fn defer_event(&self, event: &Value, next_at: i64) -> Result<()> {
        let key = field(event, "id")?;
        let mut value = event.clone();
        value.as_object_mut().context("Invalid event")?.remove("id");
        value["retries"] = json!(event["retries"].as_u64().unwrap_or(0).saturating_add(1));
        value["nextAt"] = json!(next_at);
        self.db.execute(
            "UPDATE events SET value=? WHERE id=?",
            params![serde_json::to_string(&value)?, key],
        )?;
        Ok(())
    }
    pub fn event_done(&self, key: &str) -> Result<()> {
        self.db.execute("DELETE FROM events WHERE id=?", [key])?;
        Ok(())
    }
    pub fn enroll(&self, repo: &Value) -> Result<()> {
        let name = repo_name(field(repo, "name")?)?;
        let mut value = repo.clone();
        value["name"] = json!(name);
        self.put("repos", &name, &value)
    }
    pub fn queue(&self, repo: &Value, pr: &Value, options: &Value) -> Result<Value> {
        self.tx(|store| {
            let name = field(repo,"name")?; let number = pr["number"].as_u64().context("Invalid PR number")?;
            let head = field(&pr["head"],"sha")?; let target = field(&pr["base"],"ref")?;
            let key = format!("{name}#{number}");
            let manual = options["manual"].as_bool().unwrap_or(false);
            let restart = options["restart"].as_bool().unwrap_or(false);
            let held = options["held"].as_bool().unwrap_or(false);
            let encoded: Option<String> = store.db.query_row("SELECT value FROM records WHERE kind='jobs' AND json_extract(value,'$.key')=? AND COALESCE(json_extract(value,'$.state'),'') NOT IN ('completed','superseded','cancelled') ORDER BY rowid LIMIT 1",[&key],|row| row.get(0)).optional()?;
            if let Some(encoded) = encoded {
                let mut active: Value = serde_json::from_str(&encoded)?;
                if active["head"] == head && active["target"] == target && !restart {
                    if manual && matches!(active["state"].as_str(),Some("paused" | "held")) {
                        active["resumeEpoch"] = json!(active["resumeEpoch"].as_u64().unwrap_or(0).saturating_add(1));
                        let usable = active["state"] == "held" || !truthy(&active["startedAt"]) || truthy(&active["session"]);
                        let resumable = usable || truthy(&active["report"]);
                        active["state"] = json!(if resumable { "queued" } else { "paused" });
                        active["reason"] = if resumable { Value::Null } else { json!("Restart required: no saved provider session") };
                        active["retries"] = json!(0); active["nextAt"] = json!(0);
                        if truthy(&options["trigger"]) { active["trigger"] = options["trigger"].clone(); }
                        store.put("jobs",field(&active,"id")?,&active)?;
                    }
                    return Ok(active);
                }
                active["state"] = json!("superseded"); active["updatedAt"] = json!(now());
                store.put("jobs",field(&active,"id")?,&active)?;
            }
            let timestamp = now();
            let mut job = json!({"id":id(),"key":key,"repo":name,"number":number,"head":head,"target":target,"worker":repo["worker"],
                "state":if held {"held"} else {"queued"},"manual":manual,"restart":restart,"priority":if held {0} else if manual {2} else {1},
                "resumeEpoch":0,"createdAt":timestamp,"updatedAt":timestamp,"retries":0,"nextAt":0,"session":null,"report":null});
            if let Some(trigger) = options.get("trigger") { job["trigger"] = trigger.clone(); }
            store.put("jobs",field(&job,"id")?,&job)?; Ok(job)
        })
    }
    pub fn claim(
        &self,
        worker: &str,
        exclude_ids: &[String],
        exclude_keys: &[String],
    ) -> Result<Option<Value>> {
        self.tx(|store| {
            let timestamp = now();
            let encoded: Option<String> = store.db.query_row("SELECT value FROM records WHERE kind='jobs'
                AND json_extract(value,'$.worker')=? AND json_extract(value,'$.state') IN ('queued','retrying')
                AND COALESCE(json_extract(value,'$.nextAt'),0)<=?
                AND json_extract(value,'$.id') NOT IN (SELECT value FROM json_each(?))
                AND json_extract(value,'$.key') NOT IN (SELECT value FROM json_each(?))
                ORDER BY COALESCE(json_extract(value,'$.priority'),0) DESC,rowid LIMIT 1",
                params![worker,timestamp,serde_json::to_string(exclude_ids)?,serde_json::to_string(exclude_keys)?],|row| row.get(0)).optional()?;
            let Some(encoded) = encoded else { return Ok(None); };
            let mut job: Value = serde_json::from_str(&encoded)?;
            job["state"] = json!("reviewing"); job["lease"] = json!(id());
            if job["startedAt"].is_null() { job["startedAt"] = json!(timestamp); }
            job["updatedAt"] = json!(timestamp);
            store.put("jobs",field(&job,"id")?,&job)?; Ok(Some(job))
        })
    }
    pub fn update_job(&self, key: &str, patch: &Value, lease: Option<&str>) -> Result<Value> {
        self.tx(|store| {
            let mut job = store.get("jobs", key)?.context("Review ownership lost")?;
            if let Some(lease) = lease.filter(|s| !s.is_empty())
                && (job["lease"] != lease || job["state"] != "reviewing")
            {
                bail!("Review ownership lost");
            }
            job.as_object_mut()
                .context("Invalid persisted job")?
                .extend(patch.as_object().context("Invalid job patch")?.clone());
            job["updatedAt"] = json!(now());
            store.put("jobs", key, &job)?;
            Ok(job)
        })
    }
    pub fn prune(&self, days: i64) -> Result<()> {
        let before = now().saturating_sub(days.saturating_mul(86400000));
        self.tx(|store| {
            store.db.execute(
                "DELETE FROM receipts WHERE received<? AND id NOT IN (SELECT id FROM events)",
                [before],
            )?;
            for mut job in store.all("jobs")? {
                if !terminal(&job["state"])
                    || job["updatedAt"].as_i64().unwrap_or(i64::MAX) >= before
                {
                    continue;
                }
                let object = job.as_object_mut().context("Invalid persisted job")?;
                object.remove("report");
                object.remove("patch");
                store.put("jobs", field(&job, "id")?, &job)?;
            }
            Ok(())
        })
    }
}
pub fn eligible(repo: &Value, pr: &Value) -> bool {
    pr["state"] == "open"
        && !truthy(&pr["draft"])
        && (repo["policy"] == "everyone"
            || repo["authors"].as_array().is_some_and(|authors| {
                authors.iter().any(|author| {
                    author
                        .as_str()
                        .zip(pr["user"]["login"].as_str())
                        .is_some_and(|(a, b)| a.to_lowercase() == b.to_lowercase())
                })
            }))
}
pub fn comparison_key(value: &Value) -> String {
    format!(
        "{}:{}:{}",
        value["head"].as_str().unwrap_or("undefined"),
        value["target"].as_str().unwrap_or("undefined"),
        value["base"].as_str().unwrap_or("undefined")
    )
}
pub fn retry_delay(retry: &Value, attempt: u64, provider_wait: u64) -> u64 {
    let schedule = [5000, 15000, 30000, 60000, 120000, 300000];
    let delay = if retry["mode"] == "progressive" {
        schedule[attempt.saturating_sub(1).min(5) as usize]
    } else {
        retry["delayMs"]
            .as_u64()
            .or_else(|| {
                retry["delayMs"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
                    .map(|n| n as u64)
            })
            .unwrap_or(5000)
    };
    provider_wait
        .max(delay)
        .saturating_add(rand::thread_rng().gen_range(0..500))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Store, Value, Value) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("crow.sqlite")).unwrap();
        (
            dir,
            store,
            json!({"name":"owner/repo","worker":"w","policy":"everyone"}),
            json!({"number":1,"head":{"sha":"abc"},"base":{"ref":"main"},"state":"open","user":{"login":"author"}}),
        )
    }
    #[test]
    fn replay_deduplication_and_event_identity() {
        let (_dir, store, _, _) = fixture();
        assert!(
            store
                .accept_event("a", &json!({"id":"b","type":"pull_request"}))
                .unwrap()
        );
        assert!(!store.accept_event("a", &json!({"type":"other"})).unwrap());
        let event = store.events().unwrap().remove(0);
        assert_eq!(event["id"], "a");
        store.defer_event(&event, 123).unwrap();
        let event = store.events().unwrap().remove(0);
        assert_eq!(event["retries"], 1);
        assert_eq!(event["nextAt"], 123);
        store.event_done("a").unwrap();
        assert!(store.events().unwrap().is_empty());
        assert!(!store.accept_event("a", &json!({})).unwrap());
    }
    #[test]
    fn rollback_does_not_poison_later_transactions() {
        let (_dir, store, _, _) = fixture();
        let result: Result<()> = store.tx(|s| {
            s.put("repos", "a", &json!({}))?;
            bail!("failed")
        });
        assert!(result.is_err());
        assert!(store.get("repos", "a").unwrap().is_none());
        store.tx(|s| s.put("repos", "b", &json!({}))).unwrap();
    }
    #[test]
    fn queue_deduplication_superseding_and_lease_protection() {
        let (_dir, store, repo, mut pr) = fixture();
        let first = store.queue(&repo, &pr, &json!({})).unwrap();
        assert_eq!(
            store.queue(&repo, &pr, &json!({})).unwrap()["id"],
            first["id"]
        );
        let claimed = store.claim("w", &[], &[]).unwrap().unwrap();
        assert!(store.claim("w", &[], &[]).unwrap().is_none());
        let key = first["id"].as_str().unwrap();
        let lease = claimed["lease"].as_str().unwrap();
        assert!(
            store
                .update_job(key, &json!({"session":"s"}), Some("wrong"))
                .is_err()
        );
        store
            .update_job(key, &json!({"session":"s"}), Some(lease))
            .unwrap();
        pr["head"]["sha"] = json!("new");
        let next = store.queue(&repo, &pr, &json!({})).unwrap();
        assert_ne!(next["id"], first["id"]);
        assert_eq!(
            store.get("jobs", key).unwrap().unwrap()["state"],
            "superseded"
        );
        assert!(
            store
                .update_job(key, &json!({"state":"completed"}), Some(lease))
                .is_err()
        );
    }
    #[test]
    fn priority_exclusions_and_resume_without_session() {
        let (_dir, store, repo, mut pr) = fixture();
        let a = store.queue(&repo, &pr, &json!({})).unwrap();
        pr["number"] = json!(2);
        let b = store.queue(&repo, &pr, &json!({"manual":true})).unwrap();
        assert_eq!(
            store
                .claim("w", &[b["id"].as_str().unwrap().into()], &[])
                .unwrap()
                .unwrap()["id"],
            a["id"]
        );
        assert!(
            store
                .claim("w", &[], &[b["key"].as_str().unwrap().into()])
                .unwrap()
                .is_none()
        );
        let key = b["id"].as_str().unwrap();
        store
            .update_job(key, &json!({"state":"paused","startedAt":1}), None)
            .unwrap();
        let paused = store.queue(&repo, &pr, &json!({"manual":true})).unwrap();
        assert_eq!(paused["state"], "paused");
        assert_eq!(paused["resumeEpoch"], 1);
        store
            .update_job(key, &json!({"session":"s"}), None)
            .unwrap();
        assert_eq!(
            store.queue(&repo, &pr, &json!({"manual":true})).unwrap()["state"],
            "queued"
        );
    }
    #[test]
    fn claim_is_exclusive_between_connections() {
        let (dir, store, repo, pr) = fixture();
        store.queue(&repo, &pr, &json!({})).unwrap();
        let path = dir.path().join("crow.sqlite");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    Store::new(&path)
                        .unwrap()
                        .claim("w", &[], &[])
                        .unwrap()
                        .is_some()
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap() as usize)
                .sum::<usize>(),
            1
        );
    }
    #[test]
    fn pruning_keeps_pending_receipts_and_only_discards_old_terminal_payloads() {
        let (_dir, store, repo, pr) = fixture();
        store
            .accept_event("pending", &json!({"type":"pull_request"}))
            .unwrap();
        store
            .accept_event("done", &json!({"type":"pull_request"}))
            .unwrap();
        store.event_done("done").unwrap();
        store
            .db
            .execute("UPDATE receipts SET received=0", [])
            .unwrap();
        let mut job = store.queue(&repo, &pr, &json!({})).unwrap();
        job["state"] = json!("completed");
        job["updatedAt"] = json!(1);
        job["report"] = json!({"summary":"ok"});
        job["patch"] = json!("large");
        let key = job["id"].as_str().unwrap().to_string();
        store.put("jobs", &key, &job).unwrap();
        store.prune(7).unwrap();
        let pruned = store.get("jobs", &key).unwrap().unwrap();
        assert!(pruned.get("report").is_none());
        assert!(pruned.get("patch").is_none());
        assert_eq!(pruned["head"], job["head"]);
        assert!(!store.accept_event("pending", &json!({})).unwrap());
        assert!(store.accept_event("done", &json!({})).unwrap());
    }
    #[test]
    fn panic_rolls_back_and_persisted_events_survive_reopen() {
        let (dir, store, _, _) = fixture();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = store.tx(|s| {
                s.put("repos", "abort", &json!({}))?;
                panic!("rollback");
            });
        }));
        assert!(panic.is_err());
        assert!(store.get("repos", "abort").unwrap().is_none());
        store
            .accept_event("durable", &json!({"type":"push"}))
            .unwrap();
        drop(store);
        let reopened = Store::new(&dir.path().join("crow.sqlite")).unwrap();
        assert_eq!(reopened.events().unwrap()[0]["id"], "durable");
        assert!(!reopened.accept_event("durable", &json!({})).unwrap());
    }
    #[test]
    fn lifecycle_snapshots_match_legacy_reference() {
        // Captured from the original Store implementation, excluding generated IDs and clocks.
        let steps: Value = serde_json::from_str(r#"[["queue",1,{}],["queue",1,{}],["queue",2,{"held":true}],["claim","owner/repo#1"],["queue",2,{"manual":true}],["claim"],["update",1,{"state":"paused"}],["queue",1,{"manual":true}],["update",1,{"session":"s"}],["queue",1,{"manual":true}],["claim","owner/repo#1"],["queue",2,{},"b"],["queue",3,{"manual":true}],["claim"],["update",3,{"state":"completed"}],["queue",3,{}],["update",3,{"state":"retrying","nextAt":9000000000000000}],["claim"],["queue",4,{"held":true}],["update",4,{"state":"paused","startedAt":1,"report":{"summary":"ok"}}],["queue",4,{"manual":true}],["queue",1,{"restart":true,"manual":true}]]"#).unwrap();
        let expected: Vec<String> = serde_json::from_str(r#"["1:a:queued:0","1:a:queued:0","1:a:queued:0|2:a:held:0","1:a:queued:0|2:a:held:0","1:a:queued:0|2:a:queued:1","1:a:reviewing:0|2:a:queued:1","1:a:paused:0|2:a:queued:1","1:a:paused:1|2:a:queued:1","1:a:paused:1|2:a:queued:1","1:a:queued:2|2:a:queued:1","1:a:queued:2|2:a:reviewing:1","1:a:queued:2|2:a:superseded:1|2:b:queued:0","1:a:queued:2|2:a:superseded:1|2:b:queued:0|3:a:queued:0","1:a:queued:2|2:a:superseded:1|2:b:queued:0|3:a:reviewing:0","1:a:queued:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0","1:a:queued:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:queued:0","1:a:queued:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0","1:a:reviewing:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0","1:a:reviewing:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0|4:a:held:0","1:a:reviewing:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0|4:a:paused:0","1:a:reviewing:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0|4:a:queued:1","1:a:superseded:2|2:a:superseded:1|2:b:queued:0|3:a:completed:0|3:a:retrying:0|4:a:queued:1|1:a:queued:0"]"#).unwrap();
        let (_dir, store, repo, _) = fixture();
        for (index, step) in steps.as_array().unwrap().iter().enumerate() {
            match step[0].as_str().unwrap() {
                "queue" => {
                    let pr = json!({"number":step[1],"head":{"sha":step.get(3).and_then(Value::as_str).unwrap_or("a")},"base":{"ref":"main"}});
                    store.queue(&repo, &pr, &step[2]).unwrap();
                }
                "claim" => {
                    let exclude: Vec<String> = step
                        .get(1)
                        .and_then(Value::as_str)
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                    store.claim("w", &[], &exclude).unwrap();
                }
                "update" => {
                    let job = store
                        .all("jobs")
                        .unwrap()
                        .into_iter()
                        .rfind(|j| j["number"] == step[1])
                        .unwrap();
                    store
                        .update_job(job["id"].as_str().unwrap(), &step[2], None)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let actual = store
                .all("jobs")
                .unwrap()
                .iter()
                .map(|j| {
                    format!(
                        "{}:{}:{}:{}",
                        j["number"],
                        j["head"].as_str().unwrap(),
                        j["state"].as_str().unwrap(),
                        j["resumeEpoch"]
                    )
                })
                .collect::<Vec<_>>()
                .join("|");
            assert_eq!(actual, expected[index], "step {index}: {step}");
        }
    }
    #[test]
    fn retry_and_policy_boundaries() {
        let retry = json!({"mode":"progressive"});
        assert!((5000..5500).contains(&retry_delay(&retry, 0, 0)));
        assert!((300000..300500).contains(&retry_delay(&retry, u64::MAX, 0)));
        assert_eq!(retry_delay(&retry, 1, u64::MAX), u64::MAX);
        assert!(eligible(
            &json!({"authors":["ALICE"]}),
            &json!({"state":"open","user":{"login":"alice"}})
        ));
        assert!(!eligible(
            &json!({"policy":"everyone"}),
            &json!({"state":"open","draft":true})
        ));
    }
}
