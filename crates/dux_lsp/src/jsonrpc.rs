//! The wire: JSON-RPC 2.0 messages behind `Content-Length` frames.

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::Value;

/// Read one framed message, or `None` when the editor closes the pipe.
pub fn read_message(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a message without a Content-Length header",
        ));
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

/// Answer a request with a result.
pub fn respond(writer: &mut impl Write, id: &Value, result: &Value) -> std::io::Result<()> {
    send(
        writer,
        &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

/// Answer a request with a failure.
pub fn fail(writer: &mut impl Write, id: &Value, code: i64, message: &str) -> std::io::Result<()> {
    send(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
}

/// Speak an unsolicited notification.
pub fn notify(writer: &mut impl Write, method: &str, params: &Value) -> std::io::Result<()> {
    send(
        writer,
        &serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}),
    )
}

fn send(writer: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let body = message.to_string();
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_framed_message_survives_the_round_trip() {
        let message = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let mut wire = Vec::new();
        send(&mut wire, &message).expect("writing");
        let mut reader = BufReader::new(wire.as_slice());
        let read = read_message(&mut reader)
            .expect("the wire is kind")
            .expect("a message");
        assert_eq!(read, message);
    }

    #[test]
    fn a_closed_pipe_is_not_an_error() {
        let mut reader = BufReader::new(&[][..]);
        assert!(read_message(&mut reader).expect("clean end").is_none());
    }

    #[test]
    fn a_message_without_a_length_is_an_error_not_a_hang() {
        let mut reader = BufReader::new(b"\r\n{}".as_slice());
        assert!(read_message(&mut reader).is_err());
    }
}
