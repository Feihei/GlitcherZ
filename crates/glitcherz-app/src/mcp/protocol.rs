//! JSON-RPC 2.0 framing for the MCP stdio transport (S5, spec
//! `mcp-server.md` §4).
//!
//! Hand-written on purpose (S5-D2): the transport is "one JSON object per
//! line", and the three things a library would hide — version echo, error
//! codes, handshake order — are exactly what this stage has to pin down.
//!
//! Nothing here writes to stdout; the caller owns the stream so a test can
//! drive the protocol without a pipe.

use serde_json::{Map, Value, json};

/// The client sent something that is not JSON.
pub const PARSE_ERROR: i32 = -32700;
/// The frame parsed but is not a request (not an object, no `method`, no
/// `jsonrpc`, or a batch array).
pub const INVALID_REQUEST: i32 = -32600;
/// No such method.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// The method exists but the parameters are wrong (including unknown tools).
pub const INVALID_PARAMS: i32 = -32602;

/// Protocol versions this server speaks. All three are identical for a
/// tools-only server; echoing the client's choice just saves it a downgrade.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// The version reported when the client asks for something we do not know.
pub const LATEST_VERSION: &str = "2025-06-18";

/// One parsed frame.
#[derive(Debug, Clone)]
pub struct Request {
    /// `None` for a notification (the client expects no answer).
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// Parses one line.
///
/// `Err(PARSE_ERROR)` means "not JSON"; `Err(INVALID_REQUEST)` means "JSON,
/// but not a request frame". Both are answered with `id: null` because there
/// is no id to echo.
pub fn parse(line: &str) -> Result<Request, i32> {
    let value: Value = serde_json::from_str(line).map_err(|_| PARSE_ERROR)?;
    let object = value.as_object().ok_or(INVALID_REQUEST)?;
    // JSON-RPC batch arrays are gone from MCP as of 2025-06-18; an array is
    // not an object, so it already lands here.
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or(INVALID_REQUEST)?
        .to_string();
    let params = object
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    if !params.is_object() && !params.is_null() {
        return Err(INVALID_PARAMS);
    }
    Ok(Request {
        id: object.get("id").cloned().filter(|id| !id.is_null()),
        method,
        params,
    })
}

/// Picks the version to answer `initialize` with.
pub fn negotiate(requested: Option<&str>) -> &'static str {
    match requested {
        Some(version) => SUPPORTED_VERSIONS
            .iter()
            .find(|known| **known == version)
            .copied()
            .unwrap_or(LATEST_VERSION),
        None => LATEST_VERSION,
    }
}

/// A successful response line.
pub fn result(id: &Value, result: Value) -> String {
    let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
    frame.to_string()
}

/// An error response line.
pub fn error(id: Option<&Value>, code: i32, message: &str) -> String {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "error": {"code": code, "message": message},
    });
    frame.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_carries_its_id_and_params() {
        let request = parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#).expect("parse");
        assert_eq!(request.id, Some(json!(7)));
        assert_eq!(request.method, "tools/list");
        assert_eq!(request.params, json!({}));
    }

    #[test]
    fn a_notification_has_no_id() {
        let request =
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).expect("parse");
        assert!(request.id.is_none());
    }

    #[test]
    fn a_null_id_is_a_notification() {
        let request = parse(r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#)
            .expect("parse");
        assert!(request.id.is_none());
    }

    #[test]
    fn broken_json_is_a_parse_error() {
        assert_eq!(parse("{oops").expect_err("must fail"), PARSE_ERROR);
    }

    #[test]
    fn an_object_without_a_method_is_invalid() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1}"#).expect_err("must fail"),
            INVALID_REQUEST
        );
    }

    #[test]
    fn a_batch_array_is_invalid() {
        assert_eq!(parse("[]").expect_err("must fail"), INVALID_REQUEST);
    }

    #[test]
    fn array_params_are_not_supported() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"x","params":[]}"#).expect_err("must fail"),
            INVALID_PARAMS
        );
    }

    #[test]
    fn a_known_version_is_echoed() {
        assert_eq!(negotiate(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
    }

    #[test]
    fn an_unknown_version_falls_back_to_the_latest() {
        assert_eq!(negotiate(Some("2019-01-01")), LATEST_VERSION);
        assert_eq!(negotiate(None), LATEST_VERSION);
    }

    #[test]
    fn error_frames_use_a_null_id_when_there_is_none() {
        let line = error(None, PARSE_ERROR, "bad");
        let value: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["id"], Value::Null);
        assert_eq!(value["error"]["code"], json!(PARSE_ERROR));
    }
}
