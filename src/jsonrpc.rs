//! Minimal JSON-RPC 2.0 framing and parsing for the MCP proxy.
//!
//! MCP uses line-delimited JSON-RPC 2.0 over stdio (one JSON object per
//! line, terminated by `\n`). We do *not* implement the full spec —
//! notifications, batch requests, parameter validation — because our only
//! job is "observe and forward". Specifically:
//!
//! - We parse just enough to distinguish requests from responses, extract
//!   the `id` for correlation, extract `method` and `params.name` for
//!   tool-call spans, and compute sizes.
//! - We forward the **original bytes** downstream, not a re-serialized
//!   form. This ensures we never mutate the wire (bug-for-bug compatible
//!   with whatever the child emits) and preserves byte counts.

use crate::error::{Error, Result};
use crate::span::SpanStatus;
use serde_json::Value;

/// The maximum size, in bytes, of any single JSON-RPC frame we will
/// accept. Configured by the caller — see [`Frame::parse_with_limit`].
pub const DEFAULT_MAX_FRAME_BYTES: u64 = 1_048_576;

/// One JSON-RPC frame as seen on the wire.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Original bytes (not including any trailing newline).
    pub raw: Vec<u8>,
    /// Parsed JSON.
    pub value: Value,
}

impl Frame {
    /// Parse bytes into a frame. Rejects anything larger than `max_bytes`.
    pub fn parse_with_limit(bytes: &[u8], max_bytes: u64) -> Result<Self> {
        let size = bytes.len() as u64;
        if size > max_bytes {
            return Err(Error::FrameTooLarge {
                size,
                max: max_bytes,
            });
        }
        let value: Value = serde_json::from_slice(bytes)?;
        if !value.is_object() {
            return Err(Error::InvalidJsonRpc("frame is not a JSON object".into()));
        }
        Ok(Frame {
            raw: bytes.to_vec(),
            value,
        })
    }

    /// The `jsonrpc` version string, if present.
    #[must_use]
    pub fn jsonrpc_version(&self) -> Option<&str> {
        self.value.get("jsonrpc").and_then(Value::as_str)
    }

    /// The `id` field, returned verbatim as a JSON value.
    #[must_use]
    pub fn id(&self) -> Option<Value> {
        self.value.get("id").cloned()
    }

    /// The `method` field if this is a request.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.value.get("method").and_then(Value::as_str)
    }

    /// `true` if this is a request (has method) — regardless of `id`.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.method().is_some()
    }

    /// `true` if this is a notification (method, no id).
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.is_request() && self.value.get("id").is_none()
    }

    /// `true` if this is a response (has `result` or `error`, no `method`).
    #[must_use]
    pub fn is_response(&self) -> bool {
        !self.is_request()
            && (self.value.get("result").is_some() || self.value.get("error").is_some())
    }

    /// For `tools/call` requests, extract the tool name from
    /// `params.name`. Returns `None` for anything else.
    #[must_use]
    pub fn extract_tool_name(&self) -> Option<String> {
        if self.method() != Some("tools/call") {
            return None;
        }
        self.value
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    /// Extract the raw `params.arguments` bytes for digesting. Returns
    /// `None` if the field isn't present. We go back to `self.raw` and
    /// use `serde_json` to serialize the subvalue in canonical form —
    /// this is the best we can do without re-implementing a streaming
    /// parser. The digest is stable within one mcptrace process.
    #[must_use]
    pub fn arguments_canonical(&self) -> Option<Vec<u8>> {
        let args = self
            .value
            .get("params")
            .and_then(|p| p.get("arguments"))?;
        serde_json::to_vec(args).ok()
    }

    /// For responses, return the status based on presence of an error.
    #[must_use]
    pub fn response_status(&self) -> Option<SpanStatus> {
        if !self.is_response() {
            return None;
        }
        if self.value.get("error").is_some() {
            Some(SpanStatus::Error)
        } else {
            Some(SpanStatus::Ok)
        }
    }

    /// For error responses, extract `(code, message)`.
    #[must_use]
    pub fn response_error(&self) -> Option<(i64, String)> {
        let err = self.value.get("error")?;
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Some((code, msg))
    }

    /// Length of the raw wire bytes.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.raw.len() as u64
    }
}

/// Return the JSON-RPC id formatted as a stable string key for hash maps.
/// JSON-RPC ids can be string or number or null — we need a total order.
#[must_use]
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Null => "null".into(),
        other => format!("o:{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_request() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert_eq!(f.jsonrpc_version(), Some("2.0"));
        assert_eq!(f.method(), Some("tools/call"));
        assert!(f.is_request());
        assert!(!f.is_response());
    }

    #[test]
    fn parse_valid_response_ok() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.is_response());
        assert_eq!(f.response_status(), Some(SpanStatus::Ok));
    }

    #[test]
    fn parse_valid_response_error() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.is_response());
        assert_eq!(f.response_status(), Some(SpanStatus::Error));
        let (code, msg) = f.response_error().unwrap();
        assert_eq!(code, -32601);
        assert_eq!(msg, "method not found");
    }

    #[test]
    fn parse_notification() {
        let raw = br#"{"jsonrpc":"2.0","method":"notif","params":{}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.is_notification());
        assert!(f.is_request());
    }

    #[test]
    fn parse_rejects_non_object() {
        let raw = b"[1,2,3]";
        let e = Frame::parse_with_limit(raw, 1024).unwrap_err();
        assert!(matches!(e, Error::InvalidJsonRpc(_)));
    }

    #[test]
    fn parse_rejects_malformed_json() {
        let raw = b"{not json";
        let e = Frame::parse_with_limit(raw, 1024).unwrap_err();
        assert!(matches!(e, Error::Json(_)));
    }

    #[test]
    fn parse_enforces_size_cap() {
        let big = vec![b'{'; 2000];
        let e = Frame::parse_with_limit(&big, 1024).unwrap_err();
        assert!(matches!(e, Error::FrameTooLarge { .. }));
    }

    #[test]
    fn parse_allows_exactly_max() {
        // 2 bytes for `{}` is under 2 max bytes
        let raw = b"{}";
        let f = Frame::parse_with_limit(raw, 2).unwrap();
        assert_eq!(f.byte_len(), 2);
    }

    #[test]
    fn extract_tool_name_tools_call() {
        let raw = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search","arguments":{"q":"rust"}}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert_eq!(f.extract_tool_name(), Some("search".into()));
    }

    #[test]
    fn extract_tool_name_non_tools_call_is_none() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert_eq!(f.extract_tool_name(), None);
    }

    #[test]
    fn arguments_canonical_present() {
        let raw = br#"{"method":"tools/call","params":{"name":"x","arguments":{"k":1}}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let args = f.arguments_canonical().unwrap();
        // canonical form doesn't include spaces
        assert_eq!(args, br#"{"k":1}"#);
    }

    #[test]
    fn arguments_canonical_absent() {
        let raw = br#"{"method":"ping","params":{}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.arguments_canonical().is_none());
    }

    #[test]
    fn id_is_captured_as_value() {
        let raw = br#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let id = f.id().unwrap();
        assert_eq!(id, serde_json::json!(42));
    }

    #[test]
    fn id_can_be_string() {
        let raw = br#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let id = f.id().unwrap();
        assert_eq!(id, serde_json::json!("abc"));
    }

    #[test]
    fn id_key_formats() {
        assert_eq!(id_key(&serde_json::json!(42)), "n:42");
        assert_eq!(id_key(&serde_json::json!("abc")), "s:abc");
        assert_eq!(id_key(&serde_json::json!(null)), "null");
    }

    #[test]
    fn id_key_distinguishes_string_and_number() {
        let a = id_key(&serde_json::json!("42"));
        let b = id_key(&serde_json::json!(42));
        assert_ne!(a, b);
    }

    #[test]
    fn byte_len_matches_input() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert_eq!(f.byte_len(), raw.len() as u64);
    }

    #[test]
    fn response_with_neither_result_nor_error_is_not_response() {
        let raw = br#"{"jsonrpc":"2.0","id":1}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(!f.is_response());
    }

    #[test]
    fn method_without_id_is_notification() {
        let raw = br#"{"jsonrpc":"2.0","method":"hi"}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.is_notification());
    }

    #[test]
    fn response_error_missing_fields_default_zero_empty() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"error":{}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let (code, msg) = f.response_error().unwrap();
        assert_eq!(code, 0);
        assert_eq!(msg, "");
    }

    #[test]
    fn tool_name_non_string_is_none() {
        let raw = br#"{"method":"tools/call","params":{"name":42}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        assert!(f.extract_tool_name().is_none());
    }
}
