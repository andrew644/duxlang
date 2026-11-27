//! The editor's road, end to end: a real `duxc lsp` process, real
//! LSP-framed JSON-RPC, and assertions on what comes back.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const SCRATCH_DIR: &str = env!("CARGO_TARGET_TMPDIR");

fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn request(id: i64, method: &str, params: Value) -> Vec<u8> {
    frame(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

fn notify(method: &str, params: Value) -> Vec<u8> {
    frame(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
}

fn open_document(uri: &str, text: &str) -> Vec<u8> {
    notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri, "languageId": "dux", "version": 1, "text": text}}),
    )
}

fn uri_of(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn position(line: usize, character: usize) -> Value {
    json!({"line": line, "character": character})
}

/// Send every message, close the pipe, and hear everything the server
/// says before it stops.
fn conversation(messages: &[u8]) -> Vec<Value> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the server starts");
    {
        let mut stdin = server.stdin.take().expect("a pipe to the server");
        stdin.write_all(messages).expect("the wire takes it");
        stdin.flush().expect("the wire drains");
    }
    let output = server.wait_with_output().expect("the server stops");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_frames(&output.stdout)
}

fn parse_frames(wire: &[u8]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut rest = wire;
    while let Ok(text) = std::str::from_utf8(rest) {
        let Some(header_end) = text.find("\r\n\r\n") else {
            break;
        };
        let Ok(length) = text[..header_end]
            .strip_prefix("Content-Length: ")
            .unwrap_or_default()
            .trim()
            .parse::<usize>()
        else {
            break;
        };
        let body = header_end + 4;
        if rest.len() < body + length {
            break;
        }
        messages.push(serde_json::from_slice(&rest[body..body + length]).expect("valid JSON"));
        rest = &rest[body + length..];
    }
    messages
}

fn replied(messages: &[Value], id: i64) -> &Value {
    messages
        .iter()
        .find(|message| message.get("id").and_then(Value::as_i64) == Some(id))
        .unwrap_or_else(|| panic!("no answer to request {id}: {messages:#?}"))
}

fn published(messages: &[Value]) -> Vec<Vec<Value>> {
    messages
        .iter()
        .filter(|message| {
            message.get("method") == Some(&Value::String("textDocument/publishDiagnostics".into()))
        })
        .filter_map(|message| {
            message
                .get("params")?
                .get("diagnostics")?
                .as_array()
                .cloned()
        })
        .collect()
}

#[test]
fn the_greeting_exchanges_capabilties() {
    let messages = conversation(&request(
        1,
        "initialize",
        json!({"capabilities": {}, "processId": Value::Null}),
    ));
    let answer = replied(&messages, 1);
    assert_eq!(
        answer["result"]["capabilities"]["hoverProvider"],
        json!(true),
        "the server promises to answer hover"
    );
    assert_eq!(
        answer["result"]["capabilities"]["definitionProvider"],
        json!(true)
    );
}

#[test]
fn a_broken_program_speaks_through_the_editor() {
    let dir = Path::new(SCRATCH_DIR).join("lsp_broken");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("app.dux");
    let text = "package main\n\nfn greet(name: string) -> string {\n    return $\"hello {name}\"\n}\n\nfn main() {\n    greeting := greet(\"world\")\n    println(greeting)\n    m := 1 +\n}\n";
    std::fs::write(&path, text).expect("the program is written");
    let uri = uri_of(&path);

    let mut wire = request(1, "initialize", json!({"capabilities": {}}));
    wire.extend(open_document(&uri, text));
    wire.extend(notify("exit", json!({})));
    let messages = conversation(&wire);

    let diagnostics: Vec<Value> = published(&messages).into_iter().flatten().collect();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("expected an expression"))
                && diagnostic["range"]["start"]["line"] == json!(10)
        }),
        "the dangling `+` is reported where the parser noticed: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.get("code").is_some()),
        "codes travel with the diagnostics"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            diagnostic["severity"].as_i64() == Some(1) || diagnostic["severity"].as_i64() == Some(2)
        }),
        "every diagnostic says how loud it is"
    );
}

#[test]
fn hover_and_definition_answer_for_a_name() {
    let dir = Path::new(SCRATCH_DIR).join("lsp_names");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("app.dux");
    let text = "package main\n\nfn greet(name: string) -> string {\n    return $\"hello {name}\"\n}\n\nfn main() {\n    greeting := greet(\"world\")\n    println(greeting)\n}\n";
    std::fs::write(&path, text).expect("the program is written");
    let uri = uri_of(&path);

    let mut wire = request(1, "initialize", json!({"capabilities": {}}));
    wire.extend(open_document(&uri, text));
    // The caret on `greet`'s use, on the eighth line.
    let at_greet = json!({"textDocument": {"uri": uri}, "position": position(7, 18)});
    wire.extend(request(2, "textDocument/hover", at_greet.clone()));
    wire.extend(request(3, "textDocument/definition", at_greet));
    // The caret on `greeting`, the name a binding left behind.
    let at_binding = json!({"textDocument": {"uri": uri}, "position": position(8, 14)});
    wire.extend(request(4, "textDocument/hover", at_binding.clone()));
    wire.extend(request(5, "textDocument/definition", at_binding));
    // A caret on no name at all gets nothing.
    let at_nope = json!({"textDocument": {"uri": uri}, "position": position(3, 11)});
    wire.extend(request(6, "textDocument/hover", at_nope));
    wire.extend(notify("exit", json!({})));
    let messages = conversation(&wire);

    assert_eq!(
        replied(&messages, 2)["result"]["contents"]["value"],
        json!("```dux\nfn greet(name: string) -> string\n```"),
        "hover says what the function is"
    );
    assert_eq!(
        replied(&messages, 3)["result"][0]["range"]["start"],
        json!({"line": 2, "character": 3}),
        "definition lands on the name as it was written"
    );
    assert_eq!(
        replied(&messages, 3)["result"][0]["uri"],
        json!(uri),
        "in the file it lives in"
    );
    assert!(
        replied(&messages, 4)["result"]["contents"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("greeting :=")),
        "the binding of a local is hoverable too: {:?}",
        replied(&messages, 4)
    );
    assert_eq!(
        replied(&messages, 5)["result"][0]["range"]["start"],
        json!({"line": 7, "character": 4}),
        "the binding itself, not the declaration"
    );
    assert_eq!(
        replied(&messages, 6)["result"],
        json!(null),
        "a caret on `name` inside the return asks what no one bound"
    );
}

#[test]
fn definition_walks_into_an_imported_module() {
    let dir = Path::new(SCRATCH_DIR).join("lsp_imports");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    std::fs::write(
        dir.join("halberd.dux"),
        "fn poke(n: i32) -> i32 {\n    return n * 2\n}\n",
    )
    .expect("the module is written");
    let path = dir.join("app.dux");
    let text = "package main\n\nimport \"halberd\"\n\nfn main() {\n    x := halberd.poke(2)\n    println(x)\n}\n";
    std::fs::write(&path, text).expect("the program is written");
    let uri = uri_of(&path);
    let module_uri = uri_of(&dir.join("halberd.dux"));

    let mut wire = request(1, "initialize", json!({"capabilities": {}}));
    wire.extend(open_document(&uri, text));
    // The caret on `poke`, in `halberd.poke(2)`.
    let at_poke = json!({"textDocument": {"uri": uri}, "position": position(5, 18)});
    wire.extend(request(2, "textDocument/definition", at_poke.clone()));
    wire.extend(request(3, "textDocument/hover", at_poke));
    wire.extend(notify("exit", json!({})));
    let messages = conversation(&wire);

    assert_eq!(
        replied(&messages, 2)["result"][0]["uri"],
        json!(module_uri),
        "the definition lives in the file the import named"
    );
    assert_eq!(
        replied(&messages, 2)["result"][0]["range"]["start"],
        json!({"line": 0, "character": 3}),
        "on `poke`, as it was written there"
    );
    assert!(
        replied(&messages, 3)["result"]["contents"]["value"]
            .as_str()
            .is_some_and(|value| value.contains("fn poke(n: i32) -> i32")),
        "hover reaches through the namespace: {:?}",
        replied(&messages, 3)
    );
}

#[test]
fn a_request_for_something_unheard_of_says_so() {
    let messages = conversation(&request(
        7,
        "textDocument/semantics",
        json!({"textDocument": {"uri": "file:///nope.dux"}}),
    ));
    let answer = replied(&messages, 7);
    assert_eq!(answer["error"]["code"], json!(-32601));
}

#[test]
fn a_change_to_the_buffer_re_reports_the_diagnostic() {
    let dir = Path::new(SCRATCH_DIR).join("lsp_change");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("app.dux");
    let broken = "package main\n\nfn main() {\n    m := 1 +\n}\n";
    let fixed = "package main\n\nfn main() {\n    m := 1 + 2\n    println(m)\n}\n";
    std::fs::write(&path, broken).expect("the program is written");
    let uri = uri_of(&path);

    let mut wire = request(1, "initialize", json!({"capabilities": {}}));
    wire.extend(open_document(&uri, broken));
    wire.extend(notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri}, "contentChanges": [{"text": fixed}]}),
    ));
    wire.extend(notify("exit", json!({})));

    let mut server = Command::new(env!("CARGO_BIN_EXE_duxc"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the server starts");
    {
        let mut stdin = server.stdin.take().expect("a pipe to the server");
        stdin.write_all(&wire).expect("the wire takes it");
    }
    let mut out = Vec::new();
    server
        .stdout
        .take()
        .expect("a pipe back")
        .read_to_end(&mut out)
        .expect("the server speaks");
    server.wait().expect("the server stops");
    let messages = parse_frames(&out);
    let reports = published(&messages);
    assert_eq!(reports.len(), 2, "an open and a change each report");
    assert!(
        !reports[0].is_empty(),
        "the broken text has something to say: {reports:#?}"
    );
    assert!(
        reports[1].is_empty(),
        "the fixed text says nothing against itself: {:#?}",
        reports[1]
    );
}
