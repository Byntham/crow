use crate::util::{atomic, https_url, id, integer, read_json};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("Invalid {label}"))
}
fn string<'a>(value: &'a Value, label: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("Invalid {label}"))
}
fn one_of(value: &Value, values: &[&str]) -> bool {
    value.as_str().is_some_and(|v| values.contains(&v))
}
/// Match JavaScript JSON number semantics for consumers using integer accessors.
/// Unknown fields are retained; only exact integer-valued floating numbers change representation.
pub fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_numbers),
        Value::Object(values) => values.values_mut().for_each(normalize_numbers),
        Value::Number(number) if number.is_f64() => {
            if let Some(n) = number.as_f64().filter(|n| n.fract() == 0.0) {
                if n >= i64::MIN as f64 && n < i64::MAX as f64 {
                    *number = (n as i64).into();
                } else if n >= 0.0 && n < u64::MAX as f64 {
                    *number = (n as u64).into();
                }
            }
        }
        _ => {}
    }
}
pub fn home() -> PathBuf {
    let root = std::env::var_os("CROW_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join(".local/share/crow")
        });
    let absolute = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(root)
    };
    // Resolve lexical dot segments without requiring the state directory to exist.
    let mut resolved = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}
pub fn defaults(root: &Path) -> Value {
    json!({
        "version":1,"role":"both","operator":null,"publicUrl":null,"port":8787,"bind":"127.0.0.1",
        "adminToken":format!("{}{}",id(),id()),"serviceUrl":"http://127.0.0.1:8787",
        "worker":{"id":id(),"token":format!("{}{}",id(),id()),"concurrency":3,"codex":"codex","codexHome":root.join("codex"),
            "model":null,"effort":null,"subagents":{"mode":"inherit","max":8},"retry":{"mode":"fixed","count":10,"delayMs":5000},"timeoutMs":0},
        "catchUp":{"enabled":true,"threshold":10},"auditIntervalMs":3600000,"retentionDays":7,"ingress":{"type":"funnel"},"app":null
    })
}
pub fn validate_config(value: &Value) -> Result<()> {
    let c = object(value, "configuration")?;
    let worker = &value["worker"];
    object(worker, "worker configuration")?;
    let sub = &worker["subagents"];
    object(sub, "subagent settings")?;
    let retry = &worker["retry"];
    object(retry, "retry settings")?;
    let catch_up = &value["catchUp"];
    object(catch_up, "catch-up settings")?;
    if value["version"].as_f64() != Some(1.0)
        || !one_of(&value["role"], &["both", "service", "worker"])
    {
        bail!("Unsupported configuration version or role");
    }
    for token in [&value["adminToken"], &worker["token"]] {
        if !token
            .as_str()
            .is_some_and(|s| s.encode_utf16().count() >= 32)
        {
            bail!("Crow administrative and pairing tokens need at least 32 characters");
        }
    }
    if !worker["id"].as_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 100
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    }) {
        bail!("Invalid worker identifier");
    }
    let service = url::Url::parse(string(&value["serviceUrl"], "serviceUrl")?)?;
    if !service.username().is_empty()
        || service.password().is_some()
        || service.query().is_some()
        || service.fragment().is_some()
        || service.path() != "/"
        || service.host_str().is_none()
        || !(service.scheme() == "https"
            || (service.scheme() == "http"
                && matches!(
                    service.host_str(),
                    Some("127.0.0.1" | "localhost" | "[::1]")
                )))
    {
        bail!("Worker connections need HTTPS, or HTTP on localhost");
    }
    for key in ["model", "effort"] {
        let v = worker
            .get(key)
            .context("Model and reasoning selections must be nonempty strings")?;
        if !v.is_null() && !v.as_str().is_some_and(|s| !s.trim().is_empty()) {
            bail!("Model and reasoning selections must be nonempty strings");
        }
    }
    if !catch_up["enabled"].is_boolean() {
        bail!("catchUp.enabled must be a boolean");
    }
    integer(&value["port"], 1, 65535, "port")?;
    integer(&worker["concurrency"], 1, 100, "worker.concurrency")?;
    integer(&sub["max"], 0, 100, "subagents.max")?;
    if !one_of(&sub["mode"], &["inherit", "configured"]) {
        bail!("Subagent mode must be inherit or configured");
    }
    if sub["mode"] == "configured"
        && ["model", "effort"]
            .iter()
            .any(|k| !sub[*k].as_str().is_some_and(|s| !s.trim().is_empty()))
    {
        bail!("Configured subagents need model and effort");
    }
    if !one_of(&retry["mode"], &["fixed", "progressive"]) {
        bail!("Unknown retry mode");
    }
    integer(&retry["count"], 0, 100, "retry.count")?;
    integer(&retry["delayMs"], 1, 86400000, "retry.delayMs")?;
    integer(&worker["timeoutMs"], 0, 604800000, "timeoutMs")?;
    integer(&catch_up["threshold"], 1, 10000, "catchUp.threshold")?;
    integer(&value["retentionDays"], 1, 36500, "retentionDays")?;
    integer(
        &value["auditIntervalMs"],
        60000,
        604800000,
        "auditIntervalMs",
    )?;
    for key in ["bind", "serviceUrl"] {
        string(&value[key], key)?;
    }
    for key in ["codex", "codexHome"] {
        string(&worker[key], &format!("worker.{key}"))?;
    }
    for key in ["operator", "publicUrl"] {
        let v = c.get(key).with_context(|| format!("Invalid {key}"))?;
        if !v.is_null() {
            string(v, key)?;
        }
    }
    if !value["publicUrl"].is_null() {
        https_url(string(&value["publicUrl"], "publicUrl")?)?;
    }
    let ingress = &value["ingress"];
    object(ingress, "ingress")?;
    if !one_of(&ingress["type"], &["funnel", "cloudflare", "existing"]) {
        bail!("Invalid ingress type");
    }
    let app = c.get("app").context("Invalid GitHub App")?;
    if !app.is_null() {
        object(app, "GitHub App")?;
        if !app["id"].is_number() && !app["id"].is_string() {
            bail!("Invalid GitHub App ID");
        }
        for key in ["pem", "webhookSecret", "slug"] {
            string(&app[key], &format!("app.{key}"))?;
        }
        if app
            .get("botId")
            .is_some_and(|v| !v.is_null() && !v.is_number())
        {
            bail!("Invalid GitHub bot ID");
        }
    }
    if worker.get("detached").is_some_and(|v| !v.is_boolean()) {
        bail!("Invalid worker.detached");
    }
    for key in ["model", "effort"] {
        if let Some(v) = sub.get(key) {
            string(v, &format!("subagents.{key}"))?;
        }
    }
    if let Some(v) = ingress.get("port") {
        integer(v, 1, 65535, "ingress.port")?;
    }
    for key in ["target", "file", "tunnel"] {
        if let Some(v) = ingress.get(key) {
            string(v, &format!("ingress.{key}"))?;
        }
    }
    if ingress.get("pending").is_some_and(|v| !v.is_boolean()) {
        bail!("Invalid ingress.pending");
    }
    if let Some(registration) = c.get("appRegistration") {
        object(registration, "App registration")?;
        if !one_of(&registration["ownerType"], &["personal", "organization"]) {
            bail!("Invalid App owner type");
        }
        if !one_of(&registration["visibility"], &["public", "private"]) {
            bail!("Invalid App visibility");
        }
        if let Some(v) = registration.get("organization") {
            string(v, "App organization")?;
        }
    }
    Ok(())
}
pub fn load(root: &Path) -> Result<Value> {
    let mut value = read_json(&root.join("config.json"))?
        .filter(|v| !v.is_null())
        .context("Crow is not configured. Run crow setup.")?;
    normalize_numbers(&mut value);
    validate_config(&value)?;
    Ok(value)
}
pub fn save(root: &Path, value: &Value) -> Result<()> {
    validate_config(value)?;
    let mut value = value.clone();
    normalize_numbers(&mut value);
    atomic(&root.join("config.json"), &value)
}
pub fn parse_repository_settings(value: &Value) -> Result<Value> {
    let o = object(value, "repository review settings")?;
    if o.keys()
        .any(|k| !["model", "effort", "timeoutMs", "subagents", "retry"].contains(&k.as_str()))
    {
        bail!("Unknown repository review setting");
    }
    for key in ["model", "effort"] {
        if let Some(v) = o.get(key)
            && !v.is_null()
            && !v.as_str().is_some_and(|s| !s.trim().is_empty())
        {
            bail!("Model and reasoning selections must be nonempty strings");
        }
    }
    if let Some(v) = o.get("timeoutMs") {
        integer(v, 0, 604800000, "timeoutMs")?;
    }
    if let Some(sub) = o.get("subagents") {
        object(sub, "subagent overrides")?;
        if sub
            .get("mode")
            .is_some_and(|v| !one_of(v, &["inherit", "configured"]))
        {
            bail!("Subagent mode must be inherit or configured");
        }
        if let Some(v) = sub.get("max") {
            integer(v, 0, 100, "subagents.max")?;
        }
        for key in ["model", "effort"] {
            if let Some(v) = sub.get(key) {
                string(v, &format!("subagents.{key}"))?;
            }
        }
    }
    if let Some(retry) = o.get("retry") {
        object(retry, "retry overrides")?;
        if retry
            .get("mode")
            .is_some_and(|v| !one_of(v, &["fixed", "progressive"]))
        {
            bail!("Unknown retry mode");
        }
        if let Some(v) = retry.get("count") {
            integer(v, 0, 100, "retry.count")?;
        }
        if let Some(v) = retry.get("delayMs") {
            integer(v, 1, 86400000, "retry.delayMs")?;
        }
    }
    let mut value = value.clone();
    normalize_numbers(&mut value);
    Ok(value)
}
pub fn settings(config: &Value, repo: Option<&Value>) -> Result<Value> {
    let empty = json!({});
    let overrides = parse_repository_settings(
        repo.and_then(|v| v.get("settings"))
            .filter(|v| !v.is_null())
            .unwrap_or(&empty),
    )?;
    let mut merged = config.clone();
    normalize_numbers(&mut merged);
    let worker = merged
        .get_mut("worker")
        .context("Invalid worker configuration")?;
    object(worker, "worker configuration")?;
    for key in ["model", "effort", "timeoutMs"] {
        if let Some(value) = overrides.get(key) {
            worker[key] = value.clone();
        }
    }
    for key in ["subagents", "retry"] {
        if let Some(value) = overrides.get(key) {
            let target = worker
                .get_mut(key)
                .and_then(Value::as_object_mut)
                .with_context(|| format!("Invalid {key} settings"))?;
            target.extend(object(value, key)?.clone());
        }
    }
    validate_config(&merged)?;
    Ok(merged["worker"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_roundtrip_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = defaults(dir.path());
        value["future"] = json!({"arbitrary":[1,null]});
        save(dir.path(), &value).unwrap();
        assert_eq!(load(dir.path()).unwrap(), value);
    }
    #[test]
    fn reject_unsafe_service_and_invalid_shapes() {
        let mut c = defaults(Path::new("/tmp/crow"));
        for origin in [
            "http://example.com",
            "https://a:b@example.com",
            "https://example.com/path",
            "https://example.com/?x=1",
        ] {
            c["serviceUrl"] = json!(origin);
            assert!(validate_config(&c).is_err(), "{origin}");
        }
        for origin in [
            "http://localhost:8787",
            "http://[::1]:8787",
            "https://example.com",
        ] {
            c["serviceUrl"] = json!(origin);
            validate_config(&c).unwrap();
        }
        c["worker"]["concurrency"] = json!(1.5);
        assert!(validate_config(&c).is_err());
        c["worker"]["concurrency"] = json!(1.0);
        validate_config(&c).unwrap();
        c["worker"].as_object_mut().unwrap().remove("model");
        assert!(validate_config(&c).is_err());
    }
    #[test]
    fn integer_floats_load_and_merge_as_usable_integers() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = defaults(dir.path());
        value["worker"]["concurrency"] = json!(3.0);
        value["future"] = json!({"whole":4.0,"fraction":4.5});
        atomic(&dir.path().join("config.json"), &value).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded["worker"]["concurrency"].as_u64(), Some(3));
        assert_eq!(loaded["future"]["whole"].as_u64(), Some(4));
        assert_eq!(loaded["future"]["fraction"].as_f64(), Some(4.5));
        let worker = settings(
            &value,
            Some(&json!({"settings":{"retry":{"delayMs":1000.0}}})),
        )
        .unwrap();
        assert_eq!(worker["concurrency"].as_u64(), Some(3));
        assert_eq!(worker["retry"]["delayMs"].as_u64(), Some(1000));
    }
    #[test]
    fn overrides_are_partial_and_null_clears_model() {
        let mut config = defaults(Path::new("/tmp/crow"));
        config["worker"]["model"] = json!("model-1");
        let result = settings(
            &config,
            Some(&json!({"settings":{"model":null,"retry":{"count":2},"subagents":{"max":0}}})),
        )
        .unwrap();
        assert!(result["model"].is_null());
        assert_eq!(result["retry"]["count"], 2);
        assert_eq!(result["retry"]["delayMs"], 5000);
        assert_eq!(config["worker"]["retry"]["count"], 10);
        assert!(
            settings(
                &config,
                Some(&json!({"settings":{"subagents":{"mode":"configured"}}}))
            )
            .is_err()
        );
        assert!(parse_repository_settings(&json!({"token":"secret"})).is_err());
        assert!(parse_repository_settings(&json!({"retry":null})).is_err());
    }
}
