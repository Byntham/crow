//! Exercise the actual executable's private MCP transport, not just tool helpers.
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn mcp_advertises_pagination_and_serializes_errors_without_executing_tools() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    std::fs::write(&source,json!({"dir":dir.path(),"head":"a".repeat(40),"base":"b".repeat(40),"targetSha":"b".repeat(40)}).to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_crow"))
        .arg("_inspection-mcp")
        .arg(source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"shell","arguments":{"command":"touch SHOULD_NOT_EXIST"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":5,"method":"unsupported"}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"../secret"}}}),
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"not-json\n").unwrap();
        for request in requests {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0]["result"]["protocolVersion"], "2024-11-05");
    let definitions = messages[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(definitions.len(), 4);
    for name in ["list_files", "diff"] {
        let tool = definitions.iter().find(|t| t["name"] == name).unwrap();
        assert_eq!(tool["inputSchema"]["properties"]["offset"]["minimum"], 0);
        assert!(tool["description"].as_str().unwrap().contains("nextOffset"));
    }
    assert_eq!(messages[2]["result"]["isError"], true);
    assert_eq!(
        messages[2]["result"]["content"][0]["text"],
        "Unknown inspection tool"
    );
    assert_eq!(messages[3]["result"], json!({}));
    assert_eq!(messages[4]["error"]["code"], -32601);
    assert_eq!(
        messages[5]["result"]["content"][0]["text"],
        "Invalid repository path"
    );
    assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
}
