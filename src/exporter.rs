//! Span exporters.
//!
//! An [`Exporter`] is anything that knows how to ship a batch of spans to
//! an external sink. All built-in exporters honor a client timeout
//! (default 5 seconds) and never retry — if an export fails, the batch
//! is dropped and the failure is logged. This matches the observability
//! philosophy "data-plane integrity > observation completeness".
//!
//! # Built-ins
//!
//! - [`StdoutExporter`] — pretty JSON per span. For local dev.
//! - [`ZipkinExporter`] — POST to `/api/v2/spans` (Zipkin v2 JSON).
//! - [`OtlpJsonExporter`] — POST to `/v1/traces` (OTLP HTTP + JSON).
//!
//! Jaeger is **not** included as a dedicated exporter: modern Jaeger
//! accepts OTLP JSON, and writing a full thrift-over-HTTP client for
//! v1 is out of scope. Use OTLP instead.

use crate::error::{Error, Result};
use crate::span::{Span, SpanStatus};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

/// Abstract span shipper.
pub trait Exporter: Send + Sync {
    /// Stable name, used for logs and CLI option parsing.
    fn name(&self) -> &'static str;
    /// Ship a batch. Must not panic.
    fn export_batch(&self, spans: &[Span]) -> Result<()>;
}

/// Pretty-print spans to stdout as one JSON line per span.
#[derive(Debug, Default)]
pub struct StdoutExporter {
    lock: Mutex<()>,
}

impl StdoutExporter {
    /// Construct a new stdout exporter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Render a single span as a pretty one-line JSON suitable for logs.
    pub fn render_line(span: &Span) -> Result<String> {
        let v = json!({
            "t": "span",
            "ts": span.start_unix_nanos,
            "trace_id": span.trace_id,
            "span_id": span.span_id,
            "method": span.method,
            "tool": span.tool_name,
            "status": span.status.as_str(),
            "duration_ms": span.duration_ms,
            "req_bytes": span.request_bytes,
            "res_bytes": span.response_bytes,
            "arg_digest": span.arg_digest,
            "error_code": span.error_code,
        });
        Ok(serde_json::to_string(&v)?)
    }
}

impl Exporter for StdoutExporter {
    fn name(&self) -> &'static str {
        "stdout"
    }
    fn export_batch(&self, spans: &[Span]) -> Result<()> {
        let _g = self
            .lock
            .lock()
            .map_err(|e| Error::Observation(format!("stdout exporter mutex poisoned: {e}")))?;
        let mut out = std::io::stdout().lock();
        for span in spans {
            let line = Self::render_line(span)?;
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        Ok(())
    }
}

// --- Zipkin -----------------------------------------------------------------

/// POST spans to a Zipkin `/api/v2/spans` endpoint.
#[derive(Debug)]
pub struct ZipkinExporter {
    endpoint: String,
    client: reqwest::blocking::Client,
    service_name: String,
}

impl ZipkinExporter {
    /// Build a Zipkin exporter. `endpoint` is the *full* URL, e.g.
    /// `http://localhost:9411/api/v2/spans`.
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(ZipkinExporter {
            endpoint: endpoint.into(),
            client,
            service_name: service_name.into(),
        })
    }

    /// Render a batch to the Zipkin v2 JSON payload shape.
    #[must_use]
    pub fn render_payload(&self, spans: &[Span]) -> Value {
        let arr: Vec<Value> = spans.iter().map(|s| self.render_span(s)).collect();
        Value::Array(arr)
    }

    fn render_span(&self, s: &Span) -> Value {
        let ts_micros = s.start_unix_nanos / 1_000;
        let dur_micros = u128::from(s.duration_ms) * 1_000;
        let mut tags = serde_json::Map::new();
        tags.insert("mcp.method".into(), json!(s.method));
        if let Some(t) = &s.tool_name {
            tags.insert("mcp.tool".into(), json!(t));
        }
        if let Some(d) = &s.arg_digest {
            tags.insert("mcp.arg_digest".into(), json!(d));
        }
        tags.insert("mcp.request_bytes".into(), json!(s.request_bytes));
        tags.insert("mcp.response_bytes".into(), json!(s.response_bytes));
        tags.insert("mcp.status".into(), json!(s.status.as_str()));
        if let Some(code) = s.error_code {
            tags.insert("mcp.error_code".into(), json!(code));
        }
        let mut obj = serde_json::Map::new();
        obj.insert("traceId".into(), json!(s.trace_id));
        obj.insert("id".into(), json!(s.span_id));
        if let Some(p) = &s.parent_span_id {
            obj.insert("parentId".into(), json!(p));
        }
        obj.insert(
            "name".into(),
            json!(s.tool_name.clone().unwrap_or_else(|| s.method.clone())),
        );
        obj.insert("kind".into(), json!("CLIENT"));
        obj.insert("timestamp".into(), json!(ts_micros as u64));
        obj.insert("duration".into(), json!(dur_micros as u64));
        obj.insert(
            "localEndpoint".into(),
            json!({ "serviceName": self.service_name }),
        );
        obj.insert("tags".into(), Value::Object(tags));
        if matches!(s.status, SpanStatus::Error | SpanStatus::Timeout) {
            obj.insert("annotations".into(), json!([]));
        }
        Value::Object(obj)
    }
}

impl Exporter for ZipkinExporter {
    fn name(&self) -> &'static str {
        "zipkin"
    }
    fn export_batch(&self, spans: &[Span]) -> Result<()> {
        if spans.is_empty() {
            return Ok(());
        }
        let payload = self.render_payload(spans);
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .map_err(|e| Error::Http(format!("zipkin post: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(format!("zipkin returned {}", resp.status())));
        }
        Ok(())
    }
}

// --- OTLP JSON --------------------------------------------------------------

/// POST spans to an OTLP HTTP endpoint as JSON.
///
/// The envelope shape matches the OTLP/HTTP+JSON spec (opentelemetry.io
/// /docs/specs/otlp/), using `resourceSpans[].scopeSpans[].spans[]`.
#[derive(Debug)]
pub struct OtlpJsonExporter {
    endpoint: String,
    client: reqwest::blocking::Client,
    service_name: String,
}

impl OtlpJsonExporter {
    /// Build an OTLP JSON exporter. `endpoint` is the full URL,
    /// e.g. `http://localhost:4318/v1/traces`.
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(OtlpJsonExporter {
            endpoint: endpoint.into(),
            client,
            service_name: service_name.into(),
        })
    }

    /// Render a batch to the OTLP JSON envelope.
    #[must_use]
    pub fn render_payload(&self, spans: &[Span]) -> Value {
        let otlp_spans: Vec<Value> = spans.iter().map(Self::render_span).collect();
        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        { "key": "service.name",
                          "value": { "stringValue": self.service_name } },
                        { "key": "telemetry.sdk.name",
                          "value": { "stringValue": "mcptrace-rs" } },
                        { "key": "telemetry.sdk.version",
                          "value": { "stringValue": env!("CARGO_PKG_VERSION") } }
                    ]
                },
                "scopeSpans": [{
                    "scope": { "name": "mcptrace-rs", "version": env!("CARGO_PKG_VERSION") },
                    "spans": otlp_spans,
                }]
            }]
        })
    }

    fn render_span(s: &Span) -> Value {
        let start_ns = s.start_unix_nanos;
        let end_ns = start_ns + u128::from(s.duration_ms) * 1_000_000;
        let status_code = match s.status {
            SpanStatus::Ok => 1,
            _ => 2,
        };
        let mut attributes: Vec<Value> = vec![
            json!({ "key": "mcp.method", "value": { "stringValue": s.method } }),
            json!({ "key": "mcp.status", "value": { "stringValue": s.status.as_str() } }),
            json!({ "key": "mcp.request_bytes", "value": { "intValue": s.request_bytes } }),
            json!({ "key": "mcp.response_bytes", "value": { "intValue": s.response_bytes } }),
        ];
        if let Some(t) = &s.tool_name {
            attributes.push(json!({ "key": "mcp.tool", "value": { "stringValue": t } }));
        }
        if let Some(d) = &s.arg_digest {
            attributes.push(json!({ "key": "mcp.arg_digest", "value": { "stringValue": d } }));
        }
        if let Some(code) = s.error_code {
            attributes.push(json!({ "key": "mcp.error_code", "value": { "intValue": code } }));
        }
        json!({
            "traceId": s.trace_id,
            "spanId": s.span_id,
            "name": s.tool_name.clone().unwrap_or_else(|| s.method.clone()),
            "kind": 3,
            "startTimeUnixNano": start_ns.to_string(),
            "endTimeUnixNano": end_ns.to_string(),
            "attributes": attributes,
            "status": { "code": status_code }
        })
    }
}

impl Exporter for OtlpJsonExporter {
    fn name(&self) -> &'static str {
        "otlp-json"
    }
    fn export_batch(&self, spans: &[Span]) -> Result<()> {
        if spans.is_empty() {
            return Ok(());
        }
        let payload = self.render_payload(spans);
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .map_err(|e| Error::Http(format!("otlp post: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(format!("otlp returned {}", resp.status())));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanStatus};

    fn sample() -> Span {
        Span::builder()
            .trace_id("0123456789abcdef0123456789abcdef")
            .span_id("fedcba9876543210")
            .method("tools/call")
            .tool_name(Some("search".into()))
            .arg_digest(Some("deadbeef".into()))
            .arg_bytes(42)
            .start_unix_nanos(1_700_000_000_000_000_000)
            .duration_ms(25)
            .request_bytes(120)
            .response_bytes(340)
            .status(SpanStatus::Ok)
            .build()
            .unwrap()
    }

    fn error_sample() -> Span {
        Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("tools/call")
            .tool_name(Some("broken".into()))
            .start_unix_nanos(1_700_000_000_000_000_000)
            .duration_ms(5)
            .error_code(Some(-32601))
            .status(SpanStatus::Error)
            .build()
            .unwrap()
    }

    #[test]
    fn stdout_render_line_has_tool_and_duration() {
        let s = sample();
        let line = StdoutExporter::render_line(&s).unwrap();
        assert!(line.contains("search"));
        assert!(line.contains("25"));
        assert!(line.contains("ok"));
    }

    #[test]
    fn stdout_render_line_is_single_line() {
        let s = sample();
        let line = StdoutExporter::render_line(&s).unwrap();
        assert!(!line.contains('\n'));
    }

    #[test]
    fn stdout_export_empty_batch() {
        let e = StdoutExporter::new();
        assert!(e.export_batch(&[]).is_ok());
    }

    #[test]
    fn stdout_name() {
        assert_eq!(StdoutExporter::new().name(), "stdout");
    }

    #[test]
    fn zipkin_render_has_client_kind() {
        let e = ZipkinExporter::new("http://localhost:9411/api/v2/spans", "mcptrace").unwrap();
        let payload = e.render_payload(&[sample()]);
        let arr = payload.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "CLIENT");
        assert_eq!(arr[0]["name"], "search");
    }

    #[test]
    fn zipkin_timestamp_and_duration_micros() {
        let e = ZipkinExporter::new("http://x/api/v2/spans", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let s = &p[0];
        assert_eq!(s["timestamp"], 1_700_000_000_000_000u64);
        assert_eq!(s["duration"], 25_000u64);
    }

    #[test]
    fn zipkin_tags_include_mcp_method_and_tool() {
        let e = ZipkinExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let tags = &p[0]["tags"];
        assert_eq!(tags["mcp.method"], "tools/call");
        assert_eq!(tags["mcp.tool"], "search");
        assert_eq!(tags["mcp.arg_digest"], "deadbeef");
    }

    #[test]
    fn zipkin_error_span_gets_error_tag() {
        let e = ZipkinExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[error_sample()]);
        let tags = &p[0]["tags"];
        assert_eq!(tags["mcp.status"], "error");
        assert_eq!(tags["mcp.error_code"], -32601);
    }

    #[test]
    fn zipkin_service_name_in_endpoint() {
        let e = ZipkinExporter::new("http://x", "my-svc").unwrap();
        let p = e.render_payload(&[sample()]);
        assert_eq!(p[0]["localEndpoint"]["serviceName"], "my-svc");
    }

    #[test]
    fn zipkin_name_is_zipkin() {
        let e = ZipkinExporter::new("http://x", "svc").unwrap();
        assert_eq!(e.name(), "zipkin");
    }

    #[test]
    fn otlp_envelope_has_resource_and_scope() {
        let e = OtlpJsonExporter::new("http://x/v1/traces", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let rs = &p["resourceSpans"][0];
        assert!(rs["resource"]["attributes"].is_array());
        assert!(rs["scopeSpans"][0]["spans"].is_array());
    }

    #[test]
    fn otlp_service_name_in_resource() {
        let e = OtlpJsonExporter::new("http://x", "my-svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let attrs = &p["resourceSpans"][0]["resource"]["attributes"];
        let svc = attrs
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "service.name")
            .unwrap();
        assert_eq!(svc["value"]["stringValue"], "my-svc");
    }

    #[test]
    fn otlp_span_kind_is_client() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let span = &p["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["kind"], 3);
    }

    #[test]
    fn otlp_span_status_ok_and_error() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        let p_ok = e.render_payload(&[sample()]);
        let p_err = e.render_payload(&[error_sample()]);
        assert_eq!(
            p_ok["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            1
        );
        assert_eq!(
            p_err["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
    }

    #[test]
    fn otlp_start_end_time_strings() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let span = &p["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["startTimeUnixNano"], "1700000000000000000");
        assert_eq!(span["endTimeUnixNano"], "1700000000025000000");
    }

    #[test]
    fn otlp_attributes_include_method() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[sample()]);
        let attrs = &p["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"];
        let found = attrs
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "mcp.method");
        assert!(found);
    }

    #[test]
    fn otlp_empty_batch_ok() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        assert!(e.export_batch(&[]).is_ok());
    }

    #[test]
    fn otlp_name() {
        let e = OtlpJsonExporter::new("http://x", "svc").unwrap();
        assert_eq!(e.name(), "otlp-json");
    }

    #[test]
    fn zipkin_empty_batch_ok() {
        let e = ZipkinExporter::new("http://x", "svc").unwrap();
        assert!(e.export_batch(&[]).is_ok());
    }

    #[test]
    fn stdout_render_error_span() {
        let line = StdoutExporter::render_line(&error_sample()).unwrap();
        assert!(line.contains("error"));
    }

    #[test]
    fn zipkin_span_without_tool_falls_back_to_method() {
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("initialize")
            .build()
            .unwrap();
        let e = ZipkinExporter::new("http://x", "svc").unwrap();
        let p = e.render_payload(&[s]);
        assert_eq!(p[0]["name"], "initialize");
    }
}
