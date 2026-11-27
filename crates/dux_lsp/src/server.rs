//! The conversation: read requests, answer them, keep the editor told.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};

use serde_json::{json, Value};

use crate::features::{self, Focus};
use crate::jsonrpc;

/// Speak the protocol until the editor says `exit` or closes the pipe.
pub fn serve(input: impl Read, mut output: impl Write) -> std::io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut open: HashMap<String, String> = HashMap::new();
    while let Some(message) = jsonrpc::read_message(&mut reader)? {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => jsonrpc::respond(
                &mut output,
                id.as_ref().unwrap_or(&Value::Null),
                &initialize_result(),
            )?,
            "initialized" | "" => {}
            "shutdown" => jsonrpc::respond(
                &mut output,
                id.as_ref().unwrap_or(&Value::Null),
                &Value::Null,
            )?,
            "exit" => break,
            "textDocument/didOpen" => {
                if let Some((uri, text)) = opened(&params) {
                    open.insert(uri.clone(), text);
                    let text = open.get(&uri).expect("just stored").clone();
                    publish(&mut output, &uri, &text)?;
                }
            }
            "textDocument/didChange" => {
                if let Some((uri, text)) = changed(&params) {
                    open.insert(uri.clone(), text);
                    let text = open.get(&uri).expect("just stored").clone();
                    publish(&mut output, &uri, &text)?;
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = document_uri(&params) {
                    open.remove(&uri);
                }
            }
            "textDocument/hover" | "textDocument/definition" => {
                let answer = ask(&open, &params);
                let result = match (method, answer) {
                    ("textDocument/hover", Some((signature, _, range))) => Some(json!({
                        "contents": {"kind": "markdown", "value": format!("```dux\n{signature}\n```")},
                        "range": range,
                    })),
                    ("textDocument/definition", Some((_, location, _))) => Some(json!([location])),
                    _ => None,
                };
                jsonrpc::respond(
                    &mut output,
                    id.as_ref().unwrap_or(&Value::Null),
                    &result.unwrap_or(Value::Null),
                )?;
            }
            _ => {
                if let Some(id) = id {
                    jsonrpc::fail(
                        &mut output,
                        &id,
                        -32601,
                        &format!("`{method}` is not a thing this server does"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": 1,
            "hoverProvider": true,
            "definitionProvider": true,
        },
        "serverInfo": {"name": "dux", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn opened(params: &Value) -> Option<(String, String)> {
    let document = params.get("textDocument")?;
    let uri = document.get("uri")?.as_str()?.to_string();
    let text = document.get("text")?.as_str()?.to_string();
    Some((uri, text))
}

fn changed(params: &Value) -> Option<(String, String)> {
    let uri = document_uri(params)?;
    // The server asked for full synchronization, so a change carries the
    // whole new text; the last change in the list is the current one.
    let text = params
        .get("contentChanges")?
        .as_array()?
        .last()?
        .get("text")?
        .as_str()?
        .to_string();
    Some((uri, text))
}

fn document_uri(params: &Value) -> Option<String> {
    Some(
        params
            .get("textDocument")?
            .get("uri")?
            .as_str()?
            .to_string(),
    )
}

/// Re-read the program as the compiler would and tell the editor what
/// came back for this file.
fn publish(output: &mut impl Write, uri: &str, text: &str) -> std::io::Result<()> {
    let diagnostics = match analyzed(uri, text) {
        Ok(analyzed) => features::diagnostics(&analyzed, uri),
        Err(message) => {
            vec![json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "severity": 1,
                "source": "dux",
                "message": message,
            })]
        }
    };
    jsonrpc::notify(
        output,
        "textDocument/publishDiagnostics",
        &json!({"uri": uri, "diagnostics": diagnostics}),
    )
}

fn analyzed(uri: &str, text: &str) -> Result<dux_driver::Analyzed, String> {
    let path = features::path_of_uri(uri)
        .ok_or_else(|| "the server only understands `file:` documents".to_string())?;
    dux_driver::analyze_text(&path, text.to_string()).map_err(|failure| {
        failure
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_else(|| "the program could not be read".to_string())
    })
}

/// Hover and go-to-definition share the question — what is this name? —
/// and differ only in the answer they keep.
fn ask(open: &HashMap<String, String>, params: &Value) -> Option<(String, Value, Value)> {
    let uri = document_uri(params)?;
    let text = open.get(&uri)?;
    let position = params.get("position")?;
    let line = position.get("line")?.as_u64()? as usize;
    let character = position.get("character")?.as_u64()? as usize;
    let focus: Focus = features::focus_at(text, line, character)?;
    let analyzed = analyzed(&uri, text).ok()?;
    let answer = features::lookup(&analyzed, &focus)?;
    let range = features::range_of(&analyzed.sources, answer.span);
    let location = json!({
        "uri": target_uri(&analyzed, answer.file),
        "range": range,
    });
    let hover_range = features::focus_range(text, &focus);
    Some((answer.signature, location, hover_range))
}

fn target_uri(analyzed: &dux_driver::Analyzed, file: dux_source::FileId) -> Value {
    features::uri_of_path(analyzed.sources.name_of(file))
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// The server reads the standard input until told to stop; this is what
/// `duxc lsp` runs.
pub fn stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = std::io::BufReader::new(stdin.lock());
    serve(reader, stdout.lock())
}
