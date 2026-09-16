//! Validated review reports and backwards-compatible GitHub publication markers.
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct OutputError {
    pub kind: &'static str,
    pub message: &'static str,
}

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}
// ECMAScript trim includes BOM and excludes Unicode NEXT LINE.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}
fn string(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn findings(v: &Value) -> &[Value] {
    v["findings"].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn output(message: &'static str) -> anyhow::Error {
    OutputError {
        kind: "output",
        message,
    }
    .into()
}

pub fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "summary":{"type":"string"},"findings":{"type":"array","items":{
            "type":"object","additionalProperties":false,"properties":{
                "title":{"type":"string"},"body":{"type":"string"},"path":{"type":"string"},
                "line":{"type":"integer"},"severity":{"enum":["critical","high","medium","low"]}
            },"required":["title","body","path","line","severity"]
        }}},"required":["summary","findings"]})
}

pub fn validate_report(input: &Value) -> Result<Value> {
    let summary = input.get("summary").and_then(Value::as_str);
    let entries = input.get("findings").and_then(Value::as_array);
    if !input.is_object()
        || summary.is_none_or(|s| js_trim(s).is_empty() || units(s) > 16_000)
        || entries.is_none_or(|f| f.len() > 100)
    {
        bail!("Invalid final review report");
    }
    let mut checked = Vec::new();
    for entry in entries.unwrap() {
        let title = entry["title"].as_str();
        let body = entry["body"].as_str();
        let path = entry["path"].as_str();
        let line = entry["line"].as_f64();
        if !entry.is_object()
            || !matches!(
                entry["severity"].as_str(),
                Some("critical" | "high" | "medium" | "low")
            )
            || title.is_none_or(|s| js_trim(s).is_empty() || units(s) > 300)
            || body.is_none_or(|s| js_trim(s).is_empty() || units(s) > 4_000)
            || line.is_none_or(|n| !n.is_finite() || n.fract() != 0.0 || n < 1.0)
            || path.is_none_or(|p| !crate::inspection::safe_path(p))
        {
            bail!("Invalid finding in final review report");
        }
        let mut f = entry.clone();
        // JSON 2.0 is an integer in JavaScript; normalize it for GitHub anchors.
        let n = line.unwrap();
        if n < u64::MAX as f64 {
            f["line"] = Value::from(n as u64);
        }
        // Hash exactly the same JSON array as the JavaScript implementation.
        f["id"] = Value::String(
            crate::util::hash(&json!([
                path.unwrap(),
                js_trim(title.unwrap()).to_lowercase()
            ]))[..20]
                .into(),
        );
        checked.push(f);
    }
    let report = json!({"summary":summary.unwrap(),"findings":checked});
    if serde_json::to_vec(&report)?.len() > 45_000
        || units(summary.unwrap()) + units(&render_findings(&report)) > 50_000
    {
        return Err(output(
            "Invalid final review report: shorten the completed report to fit GitHub publication limits",
        ));
    }
    Ok(report)
}

pub fn marker(meta: &Value) -> String {
    format!(
        "<!-- crow-review:v1 {} -->",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(meta).expect("JSON values serialize"))
    )
}

pub fn metadata(body: &str) -> Option<Value> {
    static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"<!-- crow-review:v1 ([A-Za-z0-9_-]+) -->").unwrap()
    });
    let captures = MARKER.captures(body)?;
    let bytes = URL_SAFE_NO_PAD.decode(&captures[1]).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let obj = value.as_object()?;
    for key in ["job", "head", "base", "target", "guidance"] {
        if obj.get(key).is_some_and(|v| !v.is_string()) {
            return None;
        }
    }
    for key in ["model", "effort"] {
        if obj.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) {
            return None;
        }
    }
    Some(value)
}

fn render_findings(report: &Value) -> String {
    if findings(report).is_empty() {
        return String::new();
    }
    let rendered: Vec<_> = findings(report)
        .iter()
        .map(|f| {
            format!(
                "\n- **{}: {}** ({}:{})\n\n  {}",
                string(&f["severity"]),
                string(&f["title"]),
                string(&f["path"]),
                f["line"],
                string(&f["body"]).replace('\n', "\n  ")
            )
        })
        .collect();
    format!("\n\nFindings:\n{}", rendered.join("\n"))
}

pub fn report_body(job: &Value, history: &[Value], earlier_reviews: &[Value]) -> Result<String> {
    let c = &job["comparison"];
    let root = format!("https://github.com/{}", string(&job["repo"]));
    // Missing JavaScript properties are omitted, while explicit nulls stay null.
    let mut meta = serde_json::Map::new();
    for (key, value) in [
        ("job", job.get("id")),
        ("head", c.get("head")),
        ("base", c.get("base")),
        ("target", c.get("target")),
        ("guidance", job.get("guidanceFingerprint")),
        ("model", job["settings"].get("model")),
        ("effort", job["settings"].get("effort")),
    ] {
        if let Some(value) = value {
            meta.insert(key.into(), value.clone());
        }
    }
    let head = string(&c["head"]);
    let base = string(&c["base"]);
    let mut body = format!(
        "{}\n## Crow review\n\nReviewed [{}]({root}/commit/{head}) against merge base [{}]({root}/commit/{base}) for `{}`.\n\n{}",
        marker(&Value::Object(meta)),
        head.chars().take(8).collect::<String>(),
        base.chars().take(8).collect::<String>(),
        string(&c["target"]).replace('`', ""),
        string(&job["report"]["summary"])
    );
    if findings(&job["report"]).is_empty() {
        body.push_str("\n\nNo actionable findings.");
    }
    body.push_str(&render_findings(&job["report"]));
    if units(&body) > 59_000 {
        return Err(output(
            "Invalid final review report: rendered findings are too large for GitHub",
        ));
    }
    let current: HashSet<_> = findings(&job["report"])
        .iter()
        .map(|f| string(&f["id"]))
        .collect();
    let earlier: Vec<_> = history
        .iter()
        .filter(|f| !current.contains(string(&f["id"])))
        .collect();
    let mut lines = Vec::new();
    if !earlier.is_empty() {
        lines.push(String::new());
        lines.push("Earlier findings, not reassessed:".into());
        for f in &earlier {
            lines.push(format!(
                "- [{}]({})",
                string(&f["title"]).replace(['[', ']', '\r', '\n'], " "),
                string(&f["url"])
            ));
        }
    }
    let url = |r: &Value| -> String {
        r.get("html_url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| r.get("url").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned()
    };
    if !earlier_reviews.is_empty() {
        lines.push(String::new());
        lines.push("Earlier reviews, findings not reassessed:".into());
        for (i, r) in earlier_reviews.iter().enumerate() {
            lines.push(format!("- [Earlier review {}]({})", i + 1, url(r)));
        }
    }
    if lines.is_empty() {
        return Ok(body);
    }
    let detailed = format!("\n{}", lines.join("\n"));
    if units(&body) + units(&detailed) <= 60_000 {
        body.push_str(&detailed);
        return Ok(body);
    }
    let mut seen = HashSet::new();
    let urls: Vec<_> = earlier
        .iter()
        .map(|f| string(&f["url"]).to_owned())
        .chain(earlier_reviews.iter().map(url))
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    let compact = format!(
        "\n\nEarlier reviews, findings not reassessed:\n{}",
        urls.iter()
            .enumerate()
            .map(|(i, u)| format!("- [Earlier review {}]({u})", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if units(&body) + units(&compact) <= 60_000 {
        body.push_str(&compact);
        return Ok(body);
    }
    let previous = urls
        .last()
        .filter(|s| units(s) < 500)
        .map(|s| format!("[previous review]({s}) and "))
        .unwrap_or_default();
    body.push_str(&format!("\n\nEarlier findings not reassessed. See the {previous}[PR review history]({root}/pull/{}).",job["number"]));
    Ok(body)
}

pub fn inline_comments(report: &Value, patch: &str, history: &[Value]) -> Vec<Value> {
    let mut changed: HashMap<&str, HashSet<u64>> = HashMap::new();
    let mut path = "";
    let mut line = 0u64;
    for s in patch.split('\n') {
        if let Some(p) = s.strip_prefix("+++ b/") {
            path = p;
            changed.insert(path, HashSet::new());
        } else if s.starts_with("@@ ") {
            if let Some(start) = s
                .split('+')
                .nth(1)
                .and_then(|n| n.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse().ok())
            {
                line = start;
            }
        } else if s.starts_with('+') && !s.starts_with("+++") {
            if let Some(lines) = changed.get_mut(path) {
                lines.insert(line);
            }
            line = line.saturating_add(1);
        } else if !s.starts_with('-') && !s.starts_with('\\') {
            line = line.saturating_add(1);
        }
    }
    let old: HashSet<_> = history.iter().map(|f| string(&f["id"])).collect();
    findings(report).iter().filter(|f| !old.contains(string(&f["id"])) && changed.get(string(&f["path"])).is_some_and(|lines| f["line"].as_u64().is_some_and(|n| lines.contains(&n)))).map(|f| json!({"path":f["path"],"line":f["line"],"side":"RIGHT","body":format!("**{}: {}**\n\n{}",string(&f["severity"]),string(&f["title"]),string(&f["body"]))})).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn finding() -> Value {
        json!({"title":"Missing authorization","body":"Check the caller before reading.\nOtherwise private data escapes.","path":"src/api.js","line":2,"severity":"high"})
    }
    fn report() -> Value {
        validate_report(&json!({"summary":"Review complete.","findings":[finding()]})).unwrap()
    }
    fn job() -> Value {
        json!({"id":"job1","number":3,"repo":"owner/project","comparison":{"head":"a".repeat(40),"base":"b".repeat(40),"target":"main"},"settings":{"model":"model","effort":"high"},"guidanceFingerprint":"guidance","report":report()})
    }
    #[test]
    fn validates_types_paths_and_utf16_limits() {
        for invalid in [
            Value::Null,
            json!({}),
            json!({"summary":" ","findings":[]}),
            json!({"summary":"ok","findings":{}}),
        ] {
            assert!(validate_report(&invalid).is_err());
        }
        for (key, value) in [
            ("line", json!(0)),
            ("line", json!(1.5)),
            ("path", json!("../secret")),
            ("path", json!("/absolute")),
            ("severity", json!(["high"])),
            ("title", json!("😀".repeat(151))),
            ("body", json!("😀".repeat(2001))),
        ] {
            let mut f = finding();
            f[key] = value;
            assert!(validate_report(&json!({"summary":"ok","findings":[f]})).is_err());
        }
        assert!(validate_report(&json!({"summary":"😀".repeat(8001),"findings":[]})).is_err());
        assert!(validate_report(&json!({"summary":"ok","findings":vec![finding();101]})).is_err());
        assert_eq!(
            validate_report(&json!({"summary":"Fine.","findings":[]})).unwrap(),
            json!({"summary":"Fine.","findings":[]})
        );
    }
    #[test]
    fn finding_id_preserves_javascript_hash_and_line_independence() {
        let old = report();
        let mut f = finding();
        f["line"] = json!(20);
        f["title"] = json!(" MISSING AUTHORIZATION ");
        let moved = validate_report(&json!({"summary":"Still present","findings":[f]})).unwrap();
        assert_eq!(old["findings"][0]["id"], moved["findings"][0]["id"]);
        assert_eq!(
            old["findings"][0]["id"],
            crate::util::hash_bytes(br#"["src/api.js","missing authorization"]"#)[..20]
        );
    }
    #[test]
    fn javascript_whitespace_and_float_encoded_integer_compatibility() {
        assert!(validate_report(&json!({"summary":"\u{feff}","findings":[]})).is_err());
        assert!(validate_report(&json!({"summary":"\u{0085}","findings":[]})).is_ok());
        let mut f = finding();
        f["title"] = json!("\u{feff}Missing authorization\u{feff}");
        f["line"] = json!(2.0);
        let r = validate_report(&json!({"summary":"ok","findings":[f]})).unwrap();
        assert_eq!(r["findings"][0]["id"], report()["findings"][0]["id"]);
        assert_eq!(r["findings"][0]["line"].as_u64(), Some(2));
    }
    #[test]
    fn metadata_rejects_untyped_and_malformed_payloads() {
        let meta = json!({"head":"a".repeat(40),"base":"b".repeat(40),"target":"main","job":"job1","model":null});
        assert_eq!(metadata(&format!("text\n{}", marker(&meta))), Some(meta));
        for body in [
            "ordinary comment".to_string(),
            "<!-- crow-review:v1 eA -->".to_string(),
            marker(&json!([])),
            marker(&json!({"head":null})),
            marker(&json!({"effort":5})),
        ] {
            assert_eq!(metadata(&body), None);
        }
    }
    #[test]
    fn publication_keeps_current_findings_and_unassessed_history() {
        let job = job();
        let current = job["report"]["findings"][0].clone();
        let body=report_body(&job,&[json!({"id":"old","title":"Earlier [problem]\n","url":"https://example.com/old"}),json!({"id":current["id"],"title":current["title"],"url":"https://example.com/duplicate"})],&[]).unwrap();
        assert!(body.contains("Earlier  problem  "));
        assert!(body.contains("Missing authorization"));
        assert!(body.contains("src/api.js:2"));
        assert!(!body.contains("duplicate"));
        assert_eq!(metadata(&body).unwrap()["base"], job["comparison"]["base"]);
    }
    #[test]
    fn inline_comments_only_cover_added_lines_without_prior_finding_duplicates() {
        let patch = "diff --git a/src/api.js b/src/api.js\n--- a/src/api.js\n+++ b/src/api.js\n@@ -1,2 +1,3 @@\n context\n+new\n tail\n";
        let r = report();
        let comments = inline_comments(&r, patch, &[]);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["line"], 2);
        assert_eq!(comments[0]["side"], "RIGHT");
        assert!(inline_comments(&r, patch, r["findings"].as_array().unwrap()).is_empty());
        let mut context = r;
        context["findings"][0]["line"] = json!(1);
        assert!(inline_comments(&context, patch, &[]).is_empty());
    }
    #[test]
    fn oversized_and_newline_heavy_reports_are_correctable_output_errors() {
        for (count, body) in [
            (12, "x".repeat(4000)),
            (5, format!("x{}", "\n".repeat(3999))),
        ] {
            let fs: Vec<_> = (0..count)
                .map(|i| {
                    let mut f = finding();
                    f["title"] = json!(format!("Issue {i}"));
                    f["body"] = json!(body);
                    f
                })
                .collect();
            let error = validate_report(&json!({"summary":"Complete","findings":fs})).unwrap_err();
            assert_eq!(error.downcast_ref::<OutputError>().unwrap().kind, "output");
        }
    }
    #[test]
    fn long_history_collapses_to_distinct_links_and_then_pr_history() {
        let job = job();
        let previous = "https://github.com/owner/project/pull/3#pullrequestreview-123";
        let history:Vec<_>=(0..1000).map(|i|json!({"id":format!("old-{i}"),"title":"Earlier issue ".repeat(20),"url":previous})).collect();
        let body = report_body(&job, &history, &[]).unwrap();
        assert!(units(&body) <= 60000);
        assert!(body.contains(previous));
        assert!(body.contains("Missing authorization"));
        assert!(body.contains("Earlier reviews, findings not reassessed"));
        let prior:Vec<_>=(0..2000).map(|i|json!({"url":format!("https://github.com/owner/project/pull/3#pullrequestreview-{i}")})).collect();
        let body = report_body(&job, &[], &prior).unwrap();
        assert!(units(&body) <= 60000);
        assert!(body.contains("pullrequestreview-1999"));
        assert!(body.contains("PR review history"));
    }
}
