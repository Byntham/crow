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
// Finding IDs hash trimmed titles, so this whitespace set must stay fixed:
// it includes the byte-order mark and excludes NEXT LINE (U+0085).
fn trim_text(s: &str) -> &str {
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
        || summary.is_none_or(|s| trim_text(s).is_empty() || units(s) > 16_000)
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
            || title.is_none_or(|s| trim_text(s).is_empty() || units(s) > 300)
            || body.is_none_or(|s| trim_text(s).is_empty() || units(s) > 4_000)
            || line.is_none_or(|n| !n.is_finite() || n.fract() != 0.0 || n < 1.0)
            || path.is_none_or(|p| !crate::inspection::safe_path(p))
        {
            bail!("Invalid finding in final review report");
        }
        let mut f = entry.clone();
        // Accept 2.0 as line 2 and store an integer for GitHub anchors.
        let n = line.unwrap();
        if n < u64::MAX as f64 {
            f["line"] = Value::from(n as u64);
        }
        // Finding IDs link published findings across reviews; keep this input stable.
        f["id"] = Value::String(
            crate::util::hash(&json!([
                path.unwrap(),
                trim_text(title.unwrap()).to_lowercase()
            ]))[..20]
                .into(),
        );
        checked.push(f);
    }
    let rendered: usize = checked
        .iter()
        .map(|f| units(&finding_block(f, &location(f))) + 2)
        .sum();
    let report = json!({"summary":summary.unwrap(),"findings":checked});
    if serde_json::to_vec(&report)?.len() > 45_000 || units(summary.unwrap()) + rendered > 50_000 {
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

/// Markdown doesn't render inside an HTML summary, so escape the text and keep
/// paired backticks as code.
fn summary_text(s: &str) -> String {
    let escaped = s
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ");
    let parts: Vec<_> = escaped.split('`').collect();
    let paired = (parts.len() - 1) / 2 * 2;
    let mut out = parts[0].to_owned();
    for (i, part) in parts.iter().enumerate().skip(1) {
        out.push_str(match i {
            i if i > paired => "`",
            i if i % 2 == 1 => "<code>",
            _ => "</code>",
        });
        out.push_str(part);
    }
    out
}

/// Severities from most to least severe, with how the comment shows them.
const SEVERITIES: [(&str, &str, &str); 4] = [
    ("critical", "🔴", "Critical"),
    ("high", "🟠", "High"),
    ("medium", "🟡", "Medium"),
    ("low", "🔵", "Low"),
];
fn rank(severity: &str) -> usize {
    SEVERITIES
        .iter()
        .position(|s| s.0 == severity)
        .unwrap_or(SEVERITIES.len())
}
/// A severity's icon and label.
fn severity(name: &str) -> (&'static str, &'static str) {
    SEVERITIES
        .iter()
        .find(|s| s.0 == name)
        .map_or(("⚪", "Unknown"), |s| (s.1, s.2))
}
fn worst<'a>(severities: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    severities.into_iter().min_by_key(|s| rank(s))
}
fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// One finding folded to its severity and title. `location` opens above its text.
fn finding_block(f: &Value, location: &str) -> String {
    let (icon, label) = severity(string(&f["severity"]));
    format!(
        "<details>\n<summary>{icon} <b>{label}</b> · {}</summary>\n\n{location}\n\n{}\n</details>",
        summary_text(trim_text(string(&f["title"]))),
        string(&f["body"])
    )
}
fn location(f: &Value) -> String {
    format!("`{}:{}`", string(&f["path"]).replace('`', ""), f["line"])
}
/// A repository path as a URL path, so any file name survives a Markdown link.
fn url_path(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn meta(job: &Value) -> Value {
    let c = &job["comparison"];
    // Omit missing metadata fields, but keep explicit nulls.
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
    Value::Object(meta)
}

/// A time as the comment shows it.
pub fn when(millis: i64) -> Option<String> {
    (millis > 0)
        .then(|| chrono::DateTime::from_timestamp_millis(millis))
        .flatten()
        .map(|t| t.format("%b %-d, %H:%M UTC").to_string())
}
pub fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}
pub fn commit(repo: &str, sha: &str) -> String {
    format!("[`{}`](https://github.com/{repo}/commit/{sha})", short(sha))
}

/// A report's result, as the comment's headline states it.
pub fn outcome(report: &Value) -> String {
    let fs = findings(report);
    match worst(fs.iter().map(|f| string(&f["severity"]))) {
        Some(name) => format!("{} {}", severity(name).0, plural(fs.len(), "finding")),
        None => "✅ No findings".into(),
    }
}

/// Reports kept for the comment's history row.
const RECENT: usize = 30;

/// The PR's published reports after `job`'s, whose new findings `review`
/// carries inline.
pub fn add_published(published: &Value, job: &Value, review: Option<&str>) -> Value {
    let mut recent = published["recent"].as_array().cloned().unwrap_or_default();
    let mut count = published["count"].as_u64().unwrap_or(0);
    let severities: Vec<_> = findings(&job["report"])
        .iter()
        .map(|f| f["severity"].clone())
        .collect();
    let entry = json!({"job":job["id"],"head":job["comparison"]["head"],"severities":severities,"url":review});
    // A retried publication replaces its own entry.
    match recent.iter_mut().find(|r| r["job"] == job["id"]) {
        Some(r) => *r = entry,
        None => {
            recent.push(entry);
            count += 1;
        }
    }
    let excess = recent.len().saturating_sub(RECENT);
    recent.drain(..excess);
    json!({"count":count,"recent":recent})
}

/// One dot per published report, coloured by its most severe finding.
fn history_row(job: &Value, published: &Value) -> Option<String> {
    let count = published["count"].as_u64().unwrap_or(0);
    let recent = published["recent"].as_array()?;
    if count < 2 || recent.is_empty() {
        return None;
    }
    let first = count.saturating_sub(recent.len() as u64) + 1;
    let pr = format!(
        "https://github.com/{}/pull/{}",
        string(&job["repo"]),
        job["number"]
    );
    let dots: String = recent
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let severities: Vec<_> = r["severities"]
                .as_array()
                .map(|v| v.iter().map(string).collect())
                .unwrap_or_default();
            let tally: Vec<_> = SEVERITIES
                .iter()
                .filter_map(|(name, ..)| {
                    let n = severities.iter().filter(|s| *s == name).count();
                    (n > 0).then(|| format!("{n} {name}"))
                })
                .collect();
            let tally = if tally.is_empty() {
                "no findings".to_owned()
            } else {
                tally.join(", ")
            };
            let icon = worst(severities.iter().copied()).map_or("🟢", |s| severity(s).0);
            let head = string(&r["head"]);
            let url = r["url"]
                .as_str()
                .map_or_else(|| format!("{pr}/commits/{head}"), str::to_owned);
            format!(
                "[{icon}]({url} \"Review {} · {} · {tally}\")",
                first + i as u64,
                short(head)
            )
        })
        .collect();
    let label = if recent.len() as u64 == count {
        format!("**{count} reviews**")
    } else {
        format!("**{count} reviews**, latest {}:", recent.len())
    };
    Some(format!("{label} {dots}"))
}

/// The report part of a PR's Crow comment. `records` are the PR's findings
/// after this report, linked to the inline comments that raised them, and
/// `published` the reports in its history row, this one last.
pub fn report_body(job: &Value, records: &[Value], published: &Value) -> Result<String> {
    // Validation bounds the report without links, so leave them out when they
    // would make the comment too long for GitHub.
    for links in [true, false] {
        let body = render(job, records, published, links);
        if units(&body) <= 59_000 {
            return Ok(body);
        }
    }
    Err(output(
        "Invalid final review report: rendered findings are too large for GitHub",
    ))
}

fn render(job: &Value, records: &[Value], published: &Value, links: bool) -> String {
    let c = &job["comparison"];
    let repo = string(&job["repo"]);
    let head = string(&c["head"]);
    let mut blocks: Vec<_> = ranked(&job["report"])
        .into_iter()
        .map(|f| {
            if !links {
                return finding_block(f, &location(f));
            }
            // The plain view, because a line anchor doesn't work where
            // GitHub renders a file, as it does Markdown.
            let mut at = format!(
                "[{}](https://github.com/{repo}/blob/{head}/{}?plain=1#L{})",
                location(f),
                url_path(string(&f["path"])),
                f["line"]
            );
            let thread = records
                .iter()
                .find(|r| r["id"] == f["id"])
                .and_then(|r| r["url"].as_str());
            if let Some(url) = thread {
                at.push_str(&format!(" · [Thread]({url})"));
            }
            finding_block(f, &at)
        })
        .collect();
    blocks.push(format!(
        "<details>\n<summary>Review notes</summary>\n\n{}\n</details>",
        string(&job["report"]["summary"])
    ));
    // Leaving out an earlier finding doesn't mean it was fixed.
    let current: HashSet<_> = findings(&job["report"])
        .iter()
        .map(|f| string(&f["id"]))
        .collect();
    let earlier = records
        .iter()
        .filter(|r| !current.contains(string(&r["id"])))
        .count();
    let mut row: Vec<_> = history_row(job, published)
        .filter(|_| links)
        .into_iter()
        .collect();
    if earlier > 0 {
        row.push(format!(
            "{} not rechecked",
            plural(earlier, "earlier finding")
        ));
    }
    if !row.is_empty() {
        blocks.push(row.join(" · "));
    }
    let settings = &job["settings"];
    let model: Vec<_> = [
        settings["model"].as_str().map(str::to_owned),
        settings["effort"].as_str().map(|e| format!("{e} effort")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut about = vec![format!(
        "{} against merge base {} on `{}`",
        commit(repo, head),
        commit(repo, string(&c["base"])),
        string(&c["target"]).replace('`', "")
    )];
    if !model.is_empty() {
        about.push(model.join(", "));
    }
    if let Some(trigger) = job["trigger"].as_str().filter(|t| !t.is_empty()) {
        about.push(trigger.to_owned());
    }
    // When the review finished.
    about.extend(when(job["updatedAt"].as_i64().unwrap_or(0)));
    blocks.push(format!("<sub>{}</sub>", about.join(" · ")));
    format!("{}\n{}", marker(&meta(job)), blocks.join("\n\n"))
}

/// Findings from most to least severe; equal ones keep the reviewer's order.
fn ranked(report: &Value) -> Vec<&Value> {
    let mut out: Vec<_> = findings(report).iter().collect();
    out.sort_by_key(|f| rank(string(&f["severity"])));
    out
}

/// The body of the review that carries a report's inline comments.
pub fn review_body(job: &Value, anchored: &[&Value], comment: &str) -> String {
    let icon = worst(anchored.iter().map(|f| string(&f["severity"]))).map_or("", |s| severity(s).0);
    // Only the comment publishes the report, so this marker names just the job
    // and can't count as a reviewed comparison.
    format!(
        "{}\n{icon} {} on {}. [Crow's comment]({comment}) has the full report.",
        marker(&json!({"job":job["id"]})),
        plural(anchored.len(), "new finding"),
        commit(string(&job["repo"]), string(&job["comparison"]["head"]))
    )
}

/// The PR's published findings after `report`: each keeps the link to the
/// inline comment that first raised it, and `anchored` ones link to `review`.
pub fn record_findings(
    report: &Value,
    history: Vec<Value>,
    anchored: &[&Value],
    review: Option<&str>,
) -> Vec<Value> {
    let mut records = history;
    for f in findings(report) {
        let inline = anchored.iter().any(|a| a["id"] == f["id"]);
        let existing = records.iter_mut().find(|r| r["id"] == f["id"]);
        let url = match (inline, &existing) {
            (true, _) => review.map_or(Value::Null, Value::from),
            (false, Some(r)) => r["url"].clone(),
            (false, None) => Value::Null,
        };
        let record = json!({"id":f["id"],"title":f["title"],"severity":f["severity"],"path":f["path"],"line":f["line"],"url":url});
        match existing {
            Some(r) => *r = record,
            None => records.push(record),
        }
    }
    records
}

/// New findings on added lines, which publication anchors as inline comments.
pub fn anchored<'a>(report: &'a Value, patch: &str, history: &[Value]) -> Vec<&'a Value> {
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
    findings(report)
        .iter()
        .filter(|f| {
            !old.contains(string(&f["id"]))
                && changed
                    .get(string(&f["path"]))
                    .zip(f["line"].as_u64())
                    .is_some_and(|(lines, n)| lines.contains(&n))
        })
        .collect()
}

pub fn inline_comments(anchored: &[&Value]) -> Vec<Value> {
    anchored
        .iter()
        .map(|f| {
            let (icon, label) = severity(string(&f["severity"]));
            let body = format!(
                "{icon} **{label}** · **{}**\n\n{}",
                string(&f["title"]),
                string(&f["body"])
            );
            json!({"path":f["path"],"line":f["line"],"side":"RIGHT","body":body})
        })
        .collect()
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
    fn finding_id_is_stable_and_ignores_line() {
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
    fn whitespace_and_float_encoded_integers_match_published_reports() {
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
    fn reports_fold_findings_by_severity_and_link_code_and_threads() {
        let mut job = job();
        let mut low = finding();
        low["title"] = json!("Slow `query`");
        low["severity"] = json!("low");
        low["path"] = json!("src/a b(1).js");
        let summary = "Two issues.";
        job["report"] =
            validate_report(&json!({"summary":summary,"findings":[low, finding()]})).unwrap();
        job["trigger"] = json!("Pull request updated");
        job["updatedAt"] = json!(1_791_355_320_000i64);
        let thread = "https://github.com/owner/project/pull/3#pullrequestreview-1";
        let records = [
            json!({"id":job["report"]["findings"][1]["id"],"url":thread}),
            json!({"id":"earlier","url":null}),
        ];
        let body = report_body(&job, &records, &json!({})).unwrap();
        let blob = format!("https://github.com/owner/project/blob/{}", "a".repeat(40));
        let high = body
            .find("<summary>🟠 <b>High</b> · Missing authorization</summary>")
            .unwrap();
        let low = body
            .find("<summary>🔵 <b>Low</b> · Slow <code>query</code></summary>")
            .unwrap();
        assert!(high < low);
        assert!(body.contains(&format!(
            "[`src/api.js:2`]({blob}/src/api.js?plain=1#L2) · [Thread]({thread})\n\nCheck the caller"
        )));
        assert!(body.contains(&format!(
            "[`src/a b(1).js:2`]({blob}/src/a%20b%281%29.js?plain=1#L2)\n\nCheck the caller"
        )));
        assert!(body.contains("<summary>Review notes</summary>\n\nTwo issues.\n</details>"));
        assert!(body.ends_with(
            "on `main` · model, high effort · Pull request updated · Oct 7, 06:42 UTC</sub>"
        ));
        assert!(!body.contains("reviews**"));
        assert!(body.contains("</details>\n\n1 earlier finding not rechecked\n\n<sub>"));
        assert_eq!(metadata(&body).unwrap()["base"], job["comparison"]["base"]);
        assert_eq!(outcome(&job["report"]), "🟠 2 findings");
        let mut clean = job.clone();
        clean["report"] = json!({"summary":"Fine.","findings":[]});
        let body = report_body(&clean, &[], &json!({})).unwrap();
        assert!(!body.contains("<b>"));
        assert!(body.contains("<summary>Review notes</summary>\n\nFine."));
        assert_eq!(outcome(&clean["report"]), "✅ No findings");
    }
    #[test]
    fn summaries_escape_html_and_keep_paired_code() {
        assert_eq!(
            summary_text("`a<b>` & `c`\nd `e"),
            "<code>a&lt;b&gt;</code> &amp; <code>c</code> d `e"
        );
    }
    #[test]
    fn inline_comments_only_cover_added_lines_without_prior_finding_duplicates() {
        let patch = "diff --git a/src/api.js b/src/api.js\n--- a/src/api.js\n+++ b/src/api.js\n@@ -1,2 +1,3 @@\n context\n+new\n tail\n";
        let r = report();
        let comments = inline_comments(&anchored(&r, patch, &[]));
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["line"], 2);
        assert_eq!(comments[0]["side"], "RIGHT");
        assert!(anchored(&r, patch, r["findings"].as_array().unwrap()).is_empty());
        let mut context = r;
        context["findings"][0]["line"] = json!(1);
        assert!(anchored(&context, patch, &[]).is_empty());
    }
    #[test]
    fn findings_keep_the_inline_comment_that_first_raised_them() {
        let first = "https://github.com/owner/project/pull/3#pullrequestreview-1";
        let r = report();
        let current = r["findings"].as_array().unwrap();
        let records = record_findings(&r, Vec::new(), &[&current[0]], Some(first));
        assert_eq!(records[0]["url"], first);
        assert_eq!(records[0]["severity"], "high");
        let records = record_findings(&r, records, &[], None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["url"], first);
        assert!(record_findings(&r, Vec::new(), &[], None)[0]["url"].is_null());
    }
    #[test]
    fn inline_reviews_point_to_the_comment() {
        let r = report();
        let f = &r["findings"][0];
        let body = review_body(&job(), &[f], "https://example.com/comment");
        assert_eq!(metadata(&body).unwrap(), json!({"job":"job1"}));
        assert!(body.ends_with(&format!(
            "\n🟠 1 new finding on [`aaaaaaa`](https://github.com/owner/project/commit/{}). [Crow's comment](https://example.com/comment) has the full report.",
            "a".repeat(40)
        )));
        assert!(review_body(&job(), &[f, f], "u").contains(" 2 new findings on "));
        let comments = inline_comments(&[f]);
        assert!(
            string(&comments[0]["body"])
                .starts_with("🟠 **High** · **Missing authorization**\n\nCheck the caller")
        );
    }
    #[test]
    fn oversized_reports_are_correctable_output_errors() {
        let fs: Vec<_> = (0..12)
            .map(|i| {
                let mut f = finding();
                f["title"] = json!(format!("Issue {i}"));
                f["body"] = json!("x".repeat(4000));
                f
            })
            .collect();
        let error = validate_report(&json!({"summary":"Complete","findings":fs})).unwrap_err();
        assert_eq!(error.downcast_ref::<OutputError>().unwrap().kind, "output");
    }
    #[test]
    fn links_are_left_out_when_they_would_not_fit() {
        let mut job = job();
        job["repo"] = json!(format!("{}/{}", "o".repeat(39), "r".repeat(100)));
        let fs: Vec<_> = (0..90)
            .map(|i| {
                let mut f = finding();
                f["title"] = json!(format!("Issue {i}"));
                f["path"] = json!(format!("src/{}.rs", "p".repeat(60)));
                f["body"] = json!("y".repeat(250));
                f
            })
            .collect();
        job["report"] =
            validate_report(&json!({"summary":"x".repeat(6000),"findings":fs})).unwrap();
        let mut records: Vec<_> = findings(&job["report"])
            .iter()
            .map(|f| json!({"id":f["id"],"url":"https://example.com/thread"}))
            .collect();
        records.push(json!({"id":"earlier","url":null}));
        let mut published = add_published(&json!({}), &job, None);
        published = add_published(
            &published,
            &json!({"id":"next","comparison":job["comparison"],"report":job["report"]}),
            None,
        );
        assert!(units(&render(&job, &records, &published, true)) > 59_000);
        let body = report_body(&job, &records, &published).unwrap();
        assert!(units(&body) <= 59_000);
        assert!(
            !body.contains("/blob/") && !body.contains("[Thread]") && !body.contains("reviews**")
        );
        assert!(body.contains(&format!("\n\n`src/{}.rs:2`\n\n", "p".repeat(60))));
        assert!(body.contains("\n\n1 earlier finding not rechecked\n\n<sub>"));
    }
    #[test]
    fn history_row_has_a_dot_for_each_recent_review() {
        let mut job = job();
        let review = "https://github.com/owner/project/pull/3#pullrequestreview-9";
        let mut published = json!({});
        for i in 0..40 {
            job["id"] = json!(format!("job{i}"));
            job["report"] = if i % 2 == 0 {
                report()
            } else {
                json!({"summary":"Fine.","findings":[]})
            };
            published = add_published(&published, &job, (i % 2 == 0).then_some(review));
            if i == 1 {
                let body = report_body(&job, &[], &published).unwrap();
                assert!(body.contains(&format!(
                    "**2 reviews** [🟠]({review} \"Review 1 · aaaaaaa · 1 high\")[🟢]"
                )));
            }
        }
        // A retried publication keeps its one entry.
        published = add_published(&published, &job, None);
        assert_eq!(published["count"], 40);
        assert_eq!(published["recent"].as_array().unwrap().len(), RECENT);
        let body = report_body(&job, &[], &published).unwrap();
        let commits = format!(
            "https://github.com/owner/project/pull/3/commits/{}",
            "a".repeat(40)
        );
        assert!(body.contains(&format!(
            "**40 reviews**, latest 30: [🟠]({review} \"Review 11 · aaaaaaa · 1 high\")[🟢]({commits} \"Review 12 · aaaaaaa · no findings\")"
        )));
        assert!(body.contains(&format!(
            "[🟢]({commits} \"Review 40 · aaaaaaa · no findings\")\n\n<sub>"
        )));
    }
}
