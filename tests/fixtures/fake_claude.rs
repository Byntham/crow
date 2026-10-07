//! Standalone fake Claude Code executable compiled by provider unit tests using rustc.
extern crate serde_json;
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};

const SESSION: &str = "11111111-2222-4333-8444-555555555555";
const OTHER_SESSION: &str = "99999999-2222-4333-8444-555555555555";
const FLAGS: &[&str] = &[
    "--print",
    "--output-format",
    "--input-format",
    "--json-schema",
    "--mcp-config",
    "--strict-mcp-config",
    "--permission-mode",
    "--allowedTools",
    "--tools",
    "--setting-sources",
    "--settings",
    "--disable-slash-commands",
    "--system-prompt",
    "--resume",
    "--model",
    "--effort",
];

fn send(value: Value) {
    println!("{value}");
    io::stdout().flush().unwrap();
}
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}
fn resolved(model: &str) -> &str {
    match model {
        "opus" => "claude-opus-5-5",
        "sonnet" => "claude-sonnet-5-5",
        "haiku" => "claude-haiku-4-5-20251001",
        other => other,
    }
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let behavior: Value = serde_json::from_slice(
        &std::fs::read(root.join("behavior.json")).unwrap_or(b"{}".to_vec()),
    )
    .unwrap();
    let first = args.first().map(String::as_str).unwrap_or("");
    if first == "--version" {
        println!("2.1.289 (Claude Code)");
        return;
    }
    if first == "--help" {
        for f in FLAGS {
            if behavior["oldHelp"] == true && *f == "--disable-slash-commands" {
                continue;
            }
            println!("  {f} <value>");
        }
        return;
    }
    if first == "auth" {
        if args.get(1).map(String::as_str) == Some("login") {
            return;
        }
        if behavior["unauth"] == true {
            println!(
                "{}",
                json!({"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"})
            );
            std::process::exit(1);
        }
        let status = if behavior["apiKey"] == true {
            json!({"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty","apiKeySource":"ANTHROPIC_API_KEY"})
        } else {
            json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"})
        };
        println!("{status}");
        return;
    }
    if flag(&args, "--input-format") == Some("stream-json") {
        for line in io::stdin().lock().lines() {
            let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if request["request"]["subtype"] != "initialize" {
                continue;
            }
            if behavior["offline"] == true {
                eprintln!("temporary service outage");
                std::process::exit(1);
            }
            let account = if behavior["unauth"] == true {
                json!({"tokenSource":"none","apiProvider":"firstParty"})
            } else {
                json!({"email":"operator@example.test","subscriptionType":"Claude Max","apiProvider":"firstParty"})
            };
            let efforts = json!(["low", "medium", "high", "xhigh", "max"]);
            let models = json!([
                {"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","supportsEffort":true,"supportedEffortLevels":efforts},
                {"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus","description":"Opus 5.5","supportsEffort":true,"supportedEffortLevels":efforts},
                {"value":"sonnet","resolvedModel":"claude-sonnet-5-5","displayName":"Sonnet","description":"Sonnet 5.5","supportsEffort":true,"supportedEffortLevels":efforts},
                {"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku","description":"Haiku 4.5"}
            ]);
            send(json!({"type":"system","subtype":"ui_invalidate"}));
            send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"account":account,"models":models}}}));
        }
        return;
    }
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt).unwrap();
    let invocation = json!({
        "args": args,
        "prompt": prompt,
        "cwd": std::env::current_dir().unwrap(),
        "env": std::env::vars().collect::<std::collections::BTreeMap<_, _>>(),
    });
    std::fs::write(root.join("invocation.json"), invocation.to_string()).unwrap();
    let mcp: Value = serde_json::from_slice(
        &std::fs::read(flag(&args, "--mcp-config").expect("MCP configuration")).unwrap(),
    )
    .unwrap();
    assert!(mcp["mcpServers"]["crow_inspection"]["command"].is_string());
    let session = if behavior["wrongSession"] == true {
        OTHER_SESSION
    } else {
        flag(&args, "--resume").unwrap_or(SESSION)
    };
    let model = if behavior["wrongModel"] == true {
        "claude-sonnet-5-5"
    } else {
        resolved(flag(&args, "--model").unwrap_or("default"))
    };
    let mut tools: Vec<String> = flag(&args, "--allowedTools")
        .unwrap_or("")
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    tools.push("StructuredOutput".into());
    if let Some(extra) = behavior["extraTool"].as_str() {
        tools.push(extra.into());
    }
    send(json!({
        "type": "system",
        "subtype": "init",
        "session_id": session,
        "tools": tools,
        "mcp_servers": [{"name":"crow_inspection","status":if behavior["mcpFailed"] == true {"failed"} else {"connected"}}],
        "model": model,
        "permissionMode": behavior["permissionMode"].as_str().or(flag(&args, "--permission-mode")),
        "apiKeySource": behavior["apiKeySource"].as_str().unwrap_or("none"),
    }));
    if behavior["missingSession"] == true {
        let error = format!("No conversation found with session ID: {session}");
        send(json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":[error]}));
        std::process::exit(1);
    }
    if behavior["hang"] == true {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if behavior["rateLimited"] == true {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        send(json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":now + 3600}}));
        send(json!({"type":"assistant","error":"rate_limit","message":{"content":[{"type":"text","text":"You've hit your limit"}]}}));
        send(json!({"type":"result","subtype":"success","is_error":true,"result":"You've hit your limit"}));
        std::process::exit(1);
    }
    if behavior["throttled"] == true {
        send(json!({"type":"assistant","error":"rate_limit","message":{"content":[{"type":"text","text":"Request rate limited"}]}}));
        send(json!({"type":"result","subtype":"success","is_error":true,"result":"Request rate limited"}));
        std::process::exit(1);
    }
    if let Some(message) = behavior["fail"].as_str() {
        send(json!({"type":"result","subtype":"success","is_error":true,"result":message}));
        std::process::exit(1);
    }
    send(json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"tool_use","name":"mcp__crow_inspection__diff","input":{}}]}}));
    if behavior["invalid"] == true {
        send(json!({"type":"result","subtype":"success","is_error":false,"result":"nonsense"}));
        return;
    }
    let report = json!({"summary":"No actionable findings.","findings":[]});
    send(json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"StructuredOutput","input":report}]}}));
    send(json!({"type":"result","subtype":"success","is_error":false,"result":report.to_string(),"structured_output":report,"session_id":session}));
}
