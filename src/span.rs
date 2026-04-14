//! The [`Span`] type — the single observation record produced by the proxy.
//!
//! A `Span` is intentionally small and flat. It contains **no** free-form
//! payload fields — only a sha256 digest of tool arguments and error
//! messages — which makes it safe to persist and forward across trust
//! boundaries (e.g., uploaded to a hosted Zipkin or OTLP endpoint).
//!
//! # Design notes
//!
//! - `schema_version = 1` is the current stable schema. Adding fields in
//!   v2 is a breaking change for consumers only if they use strict
//!   deserialization; our `#[serde(default)]`-annotated parser is lenient.
//! - `trace_id` is shared across all spans in a single proxy session,
//!   making it trivial to filter one run in an APM UI.
//! - `start_unix_nanos` is `u128` to survive past 2554 CE without wrap.
//!   Zipkin's epoch-micros and OTLP's epoch-nanos are both computed from
//!   this field at export time.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Status of a JSON-RPC tool call observed by the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    /// Child returned a non-error JSON-RPC response.
    Ok,
    /// Child returned a JSON-RPC error response (`error.code != 0`).
    Error,
    /// Proxy gave up waiting for a response after a timeout.
    Timeout,
    /// Proxy shut down with in-flight requests that never got a response.
    Orphan,
}

impl SpanStatus {
    /// `true` iff the span represents a healthy, successful call.
    #[must_use]
    pub fn is_ok(self) -> bool {
        matches!(self, SpanStatus::Ok)
    }

    /// Stable string representation used by exporters.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SpanStatus::Ok => "ok",
            SpanStatus::Error => "error",
            SpanStatus::Timeout => "timeout",
            SpanStatus::Orphan => "orphan",
        }
    }
}

/// A single observed JSON-RPC call.
///
/// Use [`Span::builder`] to construct safely. Direct field construction
/// is allowed but callers must remember not to put plaintext arguments
/// into any field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub schema_version: u32,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub trace_id: String,
    pub service_name: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_digest: Option<String>,
    #[serde(default)]
    pub arg_bytes: u64,
    pub start_unix_nanos: u128,
    pub duration_ms: u64,
    #[serde(default)]
    pub request_bytes: u64,
    #[serde(default)]
    pub response_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message_digest: Option<String>,
    pub status: SpanStatus,
}

/// Stateless builder for [`Span`]. Fluent, checked, and safe.
#[derive(Debug)]
pub struct SpanBuilder {
    span: Span,
}

impl Span {
    /// Current schema version emitted by [`Span::builder`].
    pub const SCHEMA_VERSION: u32 = 1;

    /// Start a new builder with default fields.
    #[must_use]
    pub fn builder() -> SpanBuilder {
        SpanBuilder {
            span: Span {
                schema_version: Self::SCHEMA_VERSION,
                span_id: String::new(),
                parent_span_id: None,
                trace_id: String::new(),
                service_name: "mcptrace".into(),
                method: String::new(),
                request_id: None,
                tool_name: None,
                arg_digest: None,
                arg_bytes: 0,
                start_unix_nanos: 0,
                duration_ms: 0,
                request_bytes: 0,
                response_bytes: 0,
                error_code: None,
                error_message_digest: None,
                status: SpanStatus::Ok,
            },
        }
    }

    /// Parse a span from a JSONL line. Returns a typed error on failure.
    pub fn from_jsonl_line(line: &str) -> Result<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(Error::InvalidConfig("empty span line".into()));
        }
        let span: Span = serde_json::from_str(line)?;
        span.validate()?;
        Ok(span)
    }

    /// Serialize to a JSONL line (no trailing newline).
    pub fn to_jsonl_line(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }

    /// Semantic validation. Checks that required fields are non-empty
    /// and that numeric fields are finite / non-negative.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version == 0 {
            return Err(Error::InvalidConfig("schema_version must be >= 1".into()));
        }
        if self.span_id.is_empty() {
            return Err(Error::InvalidConfig("span_id empty".into()));
        }
        if self.trace_id.is_empty() {
            return Err(Error::InvalidConfig("trace_id empty".into()));
        }
        if self.method.is_empty() {
            return Err(Error::InvalidConfig("method empty".into()));
        }
        // duration_ms fits in u64 and is always >= 0 by type; nothing more.
        Ok(())
    }

    /// Compute the span's end timestamp in unix nanoseconds.
    #[must_use]
    pub fn end_unix_nanos(&self) -> u128 {
        self.start_unix_nanos
            .saturating_add(u128::from(self.duration_ms) * 1_000_000)
    }

    /// Current wall-clock as unix nanoseconds, used when starting a span.
    #[must_use]
    pub fn now_unix_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    }
}

impl SpanBuilder {
    /// Assign trace id (32 hex chars).
    #[must_use]
    pub fn trace_id(mut self, s: impl Into<String>) -> Self {
        self.span.trace_id = s.into();
        self
    }
    /// Assign span id (16 hex chars).
    #[must_use]
    pub fn span_id(mut self, s: impl Into<String>) -> Self {
        self.span.span_id = s.into();
        self
    }
    /// JSON-RPC method, e.g. `"tools/call"`.
    #[must_use]
    pub fn method(mut self, s: impl Into<String>) -> Self {
        self.span.method = s.into();
        self
    }
    /// Service name attribute (emitted to exporters as `service.name`).
    #[must_use]
    pub fn service_name(mut self, s: impl Into<String>) -> Self {
        self.span.service_name = s.into();
        self
    }
    /// Tool name extracted from `params.name` for `tools/call`.
    #[must_use]
    pub fn tool_name(mut self, s: Option<String>) -> Self {
        self.span.tool_name = s;
        self
    }
    /// Sha256 hex digest of the raw arguments bytes.
    #[must_use]
    pub fn arg_digest(mut self, s: Option<String>) -> Self {
        self.span.arg_digest = s;
        self
    }
    /// Length in bytes of the raw arguments field as it appeared on the wire.
    #[must_use]
    pub fn arg_bytes(mut self, n: u64) -> Self {
        self.span.arg_bytes = n;
        self
    }
    /// JSON-RPC id field, kept verbatim for correlation.
    #[must_use]
    pub fn request_id(mut self, v: Option<serde_json::Value>) -> Self {
        self.span.request_id = v;
        self
    }
    /// Span start timestamp in unix nanoseconds.
    #[must_use]
    pub fn start_unix_nanos(mut self, n: u128) -> Self {
        self.span.start_unix_nanos = n;
        self
    }
    /// Duration in milliseconds.
    #[must_use]
    pub fn duration_ms(mut self, n: u64) -> Self {
        self.span.duration_ms = n;
        self
    }
    /// Raw request JSON byte count.
    #[must_use]
    pub fn request_bytes(mut self, n: u64) -> Self {
        self.span.request_bytes = n;
        self
    }
    /// Raw response JSON byte count.
    #[must_use]
    pub fn response_bytes(mut self, n: u64) -> Self {
        self.span.response_bytes = n;
        self
    }
    /// JSON-RPC error code, when the server returned an error.
    #[must_use]
    pub fn error_code(mut self, c: Option<i64>) -> Self {
        self.span.error_code = c;
        self
    }
    /// Sha256 digest of the error message string.
    #[must_use]
    pub fn error_message_digest(mut self, s: Option<String>) -> Self {
        self.span.error_message_digest = s;
        self
    }
    /// Final status for this span.
    #[must_use]
    pub fn status(mut self, s: SpanStatus) -> Self {
        self.span.status = s;
        self
    }

    /// Finalize the builder. Returns an error if the span fails [`Span::validate`].
    pub fn build(self) -> Result<Span> {
        self.span.validate()?;
        Ok(self.span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Span {
        Span::builder()
            .trace_id("0123456789abcdef0123456789abcdef")
            .span_id("fedcba9876543210")
            .method("tools/call")
            .tool_name(Some("search".into()))
            .arg_digest(Some("deadbeef".into()))
            .arg_bytes(42)
            .start_unix_nanos(1_700_000_000_000_000_000)
            .duration_ms(17)
            .request_bytes(100)
            .response_bytes(250)
            .status(SpanStatus::Ok)
            .build()
            .unwrap()
    }

    #[test]
    fn builder_defaults_are_valid() {
        let s = sample();
        assert_eq!(s.schema_version, 1);
        assert_eq!(s.service_name, "mcptrace");
    }

    #[test]
    fn builder_requires_trace_id() {
        let err = Span::builder()
            .method("tools/call")
            .span_id("a")
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn builder_requires_span_id() {
        let err = Span::builder()
            .method("tools/call")
            .trace_id("a")
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn builder_requires_method() {
        let err = Span::builder()
            .span_id("a")
            .trace_id("b")
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn jsonl_roundtrip() {
        let s = sample();
        let line = s.to_jsonl_line().unwrap();
        assert!(!line.contains('\n'));
        let back = Span::from_jsonl_line(&line).unwrap();
        assert_eq!(back.span_id, s.span_id);
        assert_eq!(back.trace_id, s.trace_id);
        assert_eq!(back.tool_name, s.tool_name);
        assert_eq!(back.duration_ms, s.duration_ms);
        assert_eq!(back.status, s.status);
    }

    #[test]
    fn jsonl_rejects_empty_line() {
        let e = Span::from_jsonl_line("").unwrap_err();
        assert!(matches!(e, Error::InvalidConfig(_)));
    }

    #[test]
    fn jsonl_rejects_invalid_json() {
        let e = Span::from_jsonl_line("not json").unwrap_err();
        assert!(matches!(e, Error::Json(_)));
    }

    #[test]
    fn jsonl_trims_trailing_crlf() {
        let s = sample();
        let mut line = s.to_jsonl_line().unwrap();
        line.push_str("\r\n");
        let back = Span::from_jsonl_line(&line).unwrap();
        assert_eq!(back.span_id, s.span_id);
    }

    #[test]
    fn status_variants_string() {
        assert_eq!(SpanStatus::Ok.as_str(), "ok");
        assert_eq!(SpanStatus::Error.as_str(), "error");
        assert_eq!(SpanStatus::Timeout.as_str(), "timeout");
        assert_eq!(SpanStatus::Orphan.as_str(), "orphan");
    }

    #[test]
    fn status_is_ok() {
        assert!(SpanStatus::Ok.is_ok());
        assert!(!SpanStatus::Error.is_ok());
        assert!(!SpanStatus::Timeout.is_ok());
        assert!(!SpanStatus::Orphan.is_ok());
    }

    #[test]
    fn end_unix_nanos_sums() {
        let s = sample();
        assert_eq!(
            s.end_unix_nanos(),
            1_700_000_000_000_000_000 + 17 * 1_000_000
        );
    }

    #[test]
    fn end_unix_nanos_saturates() {
        let s = Span::builder()
            .trace_id("a")
            .span_id("b")
            .method("m")
            .start_unix_nanos(u128::MAX - 100)
            .duration_ms(u64::MAX)
            .build()
            .unwrap();
        // saturating add -> u128::MAX, no panic
        assert_eq!(s.end_unix_nanos(), u128::MAX);
    }

    #[test]
    fn now_unix_nanos_is_nonzero() {
        let n = Span::now_unix_nanos();
        assert!(n > 1_700_000_000_000_000_000);
    }

    #[test]
    fn error_span_with_digest() {
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("tools/call")
            .error_code(Some(-32602))
            .error_message_digest(Some("abc".into()))
            .status(SpanStatus::Error)
            .build()
            .unwrap();
        assert_eq!(s.error_code, Some(-32602));
        assert_eq!(s.status, SpanStatus::Error);
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(Span::SCHEMA_VERSION, 1);
        assert_eq!(sample().schema_version, 1);
    }

    #[test]
    fn schema_version_zero_rejected() {
        let mut s = sample();
        s.schema_version = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn serialize_hides_none_fields() {
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("m")
            .build()
            .unwrap();
        let j = serde_json::to_value(&s).unwrap();
        // absent
        assert!(j.get("tool_name").is_none());
        assert!(j.get("error_code").is_none());
        // present
        assert!(j.get("method").is_some());
    }
}
