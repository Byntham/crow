//! Standalone fake Codex executable compiled by provider unit tests using rustc.
extern crate serde_json;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};
fn send(value: Value) {
    println!("{value}");
    io::stdout().flush().unwrap();
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(std::env::current_exe().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let behavior: Value = serde_json::from_slice(
        &std::fs::read(root.join("behavior.json")).unwrap_or(b"{}".to_vec()),
    )
    .unwrap();
    if args.iter().any(|a| a == "--version") {
        println!("codex-cli 0.154.0");
        return;
    }
    if args.iter().any(|a| a == "--help") {
        if behavior["legacyCli"] == true {
            println!("--json");
            return;
        }
        println!(
            "--json --output-schema --output-last-message --ignore-user-config --ignore-rules"
        );
        return;
    }
    if args.iter().any(|a| a == "features") {
        for f in [
            "shell_tool",
            "unified_exec",
            "apps",
            "plugins",
            "hooks",
            "view_image",
            "skip_host_skill_discovery",
            "multi_agent",
            "code_mode_host",
            "code_mode",
        ] {
            println!("{f} stable true");
        }
        return;
    }
    if args.iter().any(|a| a == "app-server") {
        for line in io::stdin().lock().lines() {
            let line = line.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            if request["id"].is_null() {
                continue;
            }
            let id = request["id"].clone();
            match request["method"].as_str().unwrap_or("") {
                "initialize" => send(json!({"id":id,"result":{"userAgent":"test"}})),
                "config/read" => {
                    let mut config = json!({});
                    for pair in args.windows(2) {
                        if pair[0] != "-c" {
                            continue;
                        }
                        let Some((key, raw)) = pair[1].split_once('=') else {
                            continue;
                        };
                        let Ok(value) = serde_json::from_str::<Value>(raw) else {
                            continue;
                        };
                        let mut target = &mut config;
                        let parts = key.split('.').collect::<Vec<_>>();
                        for p in &parts[..parts.len() - 1] {
                            if !target[*p].is_object() {
                                target[*p] = json!({});
                            }
                            target = &mut target[*p];
                        }
                        target[parts[parts.len() - 1]] = value;
                    }
                    if behavior["unsafe"] == true {
                        config["mcp_servers"]["unexpected"] = json!({"command":"bad"});
                    }
                    if behavior["missingProxyEnv"] == true {
                        config["mcp_servers"]["crow_inspection"]["env_vars"] = json!([]);
                    }
                    if behavior["wrongModel"] == true {
                        config["model"] = json!("other");
                    }
                    send(json!({"id":id,"result":{"config":config}}));
                }
                "account/read" => {
                    if behavior["accountOffline"] == true {
                        eprintln!("metadata backend unavailable");
                        std::process::exit(1);
                    }
                    let account = if behavior["unauth"] == true {
                        Value::Null
                    } else {
                        json!({"type":if behavior["api"]==true{"apiKey"}else{"chatgpt"},"planType":"pro"})
                    };
                    send(json!({"id":id,"result":{"account":account}}));
                }
                "model/list" => {
                    if let Some(message) = behavior["metadataExit"].as_str() {
                        eprintln!("{message}");
                        std::process::exit(1);
                    }

                    if behavior["offline"] == true {
                        send(json!({"id":id,"error":{"message":"temporary service outage"}}));
                    } else {
                        let more = request["params"]["cursor"].is_null();
                        send(
                            json!({"id":id,"result":{"data":[{"id":if more{"provider-default"}else{"second"},"model":if more{"provider-default"}else{"second"},"defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":if more{json!("page2")}else{Value::Null}}}),
                        );
                    }
                }
                _ => send(json!({"id":id,"result":{}})),
            }
        }
    } else if args.iter().any(|a| a == "exec") {
        let mut prompt = String::new();
        io::stdin().read_to_string(&mut prompt).unwrap();
        std::fs::write(root.join("invocation.json"),json!({"args":args,"prompt":prompt,"cwd":std::env::current_dir().unwrap(),"env":std::env::vars().collect::<std::collections::BTreeMap<_,_>>()} ).to_string()).unwrap();
        let session = if behavior["wrongSession"] == true {
            "other-session"
        } else {
            "saved-session"
        };
        send(json!({"type":"thread.started","thread_id":session}));
        if behavior["hang"] == true {
            std::thread::sleep(std::time::Duration::from_secs(60));
            return;
        }
        if let Some(fail) = behavior["fail"].as_str() {
            send(json!({"type":"turn.failed","error":{"message":fail}}));
            std::process::exit(1);
        }
        send(json!({"type":"item.completed","item":{"type":"mcp_tool_call","status":"completed"}}));
        let report = if behavior["invalid"] == true {
            "nonsense".into()
        } else {
            json!({"summary":"No actionable findings.","findings":[]}).to_string()
        };
        let output = &args[args
            .iter()
            .position(|a| a == "--output-last-message")
            .unwrap()
            + 1];
        std::fs::write(output, &report).unwrap();
        send(json!({"type":"item.completed","item":{"type":"agent_message","text":report}}));
        send(json!({"type":"turn.completed"}));
    }
}
