//! Scripted MCP sessions over stdio against the real binary. Every
//! stdout line must be a JSON-RPC message: a stray log line on stdout
//! breaks every MCP client.

use assert_cmd::Command;
use serde_json::{json, Value};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

fn session(args: &[&str], requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let out = Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("mcp")
        .args(args)
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}"));
            assert_eq!(v["jsonrpc"], "2.0", "{l}");
            v
        })
        .collect()
}

#[test]
fn legacy_client_session() {
    let src = fixture("structured.ndjson");
    let replies = session(
        &[&src],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"script","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_sources","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"errors","arguments":{"since":"2026-01-01T00:00:00Z"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{"query":"logger=app AND msg~/^st/"}}}),
            json!({"jsonrpc":"2.0","id":6,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":7,"method":"prompts/get"}),
        ],
    );
    // The notification gets no reply.
    assert_eq!(replies.len(), 7);
    let ids: Vec<i64> = replies.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 6);
    assert_eq!(
        replies[2]["result"]["structuredContent"]["sources"][0]["name"],
        "structured.ndjson"
    );
    let errors = &replies[3]["result"]["structuredContent"];
    assert_eq!(errors["total_matches"], 1);
    assert_eq!(errors["hits"][0]["msg"], "boom");
    assert_eq!(
        replies[4]["result"]["structuredContent"]["hits"][0]["msg"],
        "start"
    );
    assert!(replies[5]["result"].is_object());
    assert_eq!(replies[6]["error"]["code"], -32601);
}

#[test]
fn modern_client_session() {
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let src = fixture("structured.ndjson");
    let replies = session(
        &[&src],
        &[
            json!({"jsonrpc":"2.0","id":"d","method":"server/discover","params":{"_meta": meta}}),
            json!({"jsonrpc":"2.0","id":"t","method":"tools/call","params":{"name":"tail","arguments":{"source":"structured.ndjson","lines":1},"_meta": meta}}),
            json!({"jsonrpc":"2.0","id":"v","method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01"}}}),
        ],
    );
    assert_eq!(replies[0]["result"]["resultType"], "complete");
    assert_eq!(replies[0]["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(replies[1]["result"]["resultType"], "complete");
    assert_eq!(
        replies[1]["result"]["structuredContent"]["hits"][0]["msg"],
        "done"
    );
    assert_eq!(replies[2]["error"]["code"], -32022);
}

#[test]
fn garbage_input_gets_errors_not_a_crash() {
    let input = "this is not json\n[]\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    let out = Command::cargo_bin("mgi-pulse")
        .unwrap()
        .arg("mcp")
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let replies: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["error"]["code"], -32600);
    assert_eq!(replies[2]["id"], 1);
}

#[test]
fn stdin_cannot_be_a_source() {
    Command::cargo_bin("mgi-pulse")
        .unwrap()
        .args(["mcp", "-"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("stdin carries the MCP protocol"));
}
