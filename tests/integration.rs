//! End-to-end integration tests exercised via the library surface.
//!
//! These tests avoid spawning child processes (some CI environments
//! don't have `sh`) — instead they walk every module from the outside,
//! verifying that a typical operator workflow works:
//!
//! 1. Build spans and persist to a JSONL store
//! 2. Read them back
//! 3. Compute stats on the result
//! 4. Evaluate SLOs with a TOML config
//! 5. Replay them through a capture exporter

use mcptrace::digest::{arg_digest, error_digest, sha256_hex};
use mcptrace::exporter::{Exporter, OtlpJsonExporter, StdoutExporter, ZipkinExporter};
use mcptrace::jsonrpc::{id_key, Frame};
use mcptrace::replay::{load_spans, replay_to};
use mcptrace::slo::{evaluate_all, parse_config_str, SloMetric};
use mcptrace::span::{Span, SpanStatus};
use mcptrace::stats::{compute, compute_by_tool};
use mcptrace::store::{read_jsonl, JsonlStore, NullStore, SpanStore};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn mk(dur: u64, tool: &str, status: SpanStatus, ts: u128) -> Span {
    Span::builder()
        .trace_id("00000000000000000000000000000001")
        .span_id(format!("{ts:016x}"))
        .method("tools/call")
        .tool_name(Some(tool.into()))
        .arg_digest(Some(arg_digest(br#"{"q":"x"}"#)))
        .arg_bytes(8)
        .start_unix_nanos(ts)
        .duration_ms(dur)
        .request_bytes(100)
        .response_bytes(200)
        .status(status)
        .build()
        .unwrap()
}

#[test]
fn full_workflow_stores_and_reads_spans() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = JsonlStore::open(tmp.path()).unwrap();
    let base: u128 = 5_000_000_000;
    for i in 0..20 {
        store
            .record(&mk(10 + i, "search", SpanStatus::Ok, base + i as u128))
            .unwrap();
    }
    store.record(&mk(50, "write", SpanStatus::Error, base + 100)).unwrap();
    store.flush().unwrap();

    let spans = read_jsonl(tmp.path()).unwrap();
    assert_eq!(spans.len(), 21);

    // Stats
    let by_tool = compute_by_tool(&spans);
    assert!(by_tool.contains_key("search"));
    assert!(by_tool.contains_key("write"));
    assert_eq!(by_tool["search"].count, 20);
    assert_eq!(by_tool["write"].errors, 1);

    let all = compute(&spans);
    assert_eq!(all.count, 21);
    assert_eq!(all.errors, 1);
}

#[test]
fn full_workflow_slo_check_from_jsonl() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = JsonlStore::open(tmp.path()).unwrap();
    let base: u128 = 5_000_000_000;
    // 50 ok + 50 error, at target 0.9 -> burning
    for i in 0..50 {
        store
            .record(&mk(5, "t", SpanStatus::Ok, base + i as u128))
            .unwrap();
    }
    for i in 0..50 {
        store
            .record(&mk(5, "t", SpanStatus::Error, base + 100 + i as u128))
            .unwrap();
    }
    store.flush().unwrap();

    let spans = read_jsonl(tmp.path()).unwrap();

    let toml = r#"
[[slo]]
name = "errs"
metric = "error_rate"
target = 0.9
window = "10s"
burn_rate_threshold = 2.0
"#;
    let slos = parse_config_str(toml).unwrap();
    let reports = evaluate_all(&slos, &spans, base + 1_000);
    assert_eq!(reports.len(), 1);
    assert!(reports[0].burning);
    assert_eq!(reports[0].metric, SloMetric::ErrorRate);
}

#[derive(Default)]
struct CaptureExporter {
    captured: Mutex<Vec<Span>>,
}
impl Exporter for CaptureExporter {
    fn name(&self) -> &'static str {
        "capture"
    }
    fn export_batch(&self, spans: &[Span]) -> mcptrace::Result<()> {
        self.captured.lock().unwrap().extend_from_slice(spans);
        Ok(())
    }
}

#[test]
fn full_workflow_replay_to_capture_exporter() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = JsonlStore::open(tmp.path()).unwrap();
    for i in 0..5 {
        store
            .record(&mk(10, "t", SpanStatus::Ok, 1_000 + i as u128))
            .unwrap();
    }
    store.flush().unwrap();

    let spans = load_spans(tmp.path()).unwrap();
    let cap = Arc::new(CaptureExporter::default());
    let exps: Vec<Arc<dyn Exporter>> = vec![cap.clone()];
    let n = replay_to(&spans, &exps).unwrap();
    assert_eq!(n, 5);
    assert_eq!(cap.captured.lock().unwrap().len(), 5);
}

#[test]
fn request_response_correlation_via_id_key() {
    // Two agents could use the same integer ids, but in different JSON
    // types (string vs number), so id_key must distinguish them.
    let req1 = Frame::parse_with_limit(
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#,
        1024,
    )
    .unwrap();
    let req2 = Frame::parse_with_limit(
        br#"{"jsonrpc":"2.0","id":"1","method":"tools/call","params":{"name":"x"}}"#,
        1024,
    )
    .unwrap();
    assert_ne!(id_key(&req1.id().unwrap()), id_key(&req2.id().unwrap()));
}

#[test]
fn digest_never_leaks_plaintext() {
    let secret = br#"{"api_key":"sk-live-very-secret"}"#;
    let d = arg_digest(secret);
    assert_eq!(d.len(), 64);
    assert!(!d.contains("sk-live"));
    // The error-digest path should be equally safe.
    let msg = "401 Unauthorized: token=sk-live-very-secret";
    let ed = error_digest(msg);
    assert!(!ed.contains("sk-live"));
    assert_eq!(sha256_hex(msg.as_bytes()), ed);
}

#[test]
fn null_store_works_end_to_end() {
    let store = NullStore;
    let s = mk(5, "t", SpanStatus::Ok, 1_000);
    store.record(&s).unwrap();
    store.flush().unwrap();
    assert_eq!(store.name(), "null");
}

#[test]
fn zipkin_envelope_is_json_array() {
    let exp = ZipkinExporter::new("http://x/api/v2/spans", "svc").unwrap();
    let p = exp.render_payload(&[mk(5, "t", SpanStatus::Ok, 1_000)]);
    assert!(p.is_array());
    assert_eq!(exp.name(), "zipkin");
}

#[test]
fn otlp_envelope_has_expected_top_keys() {
    let exp = OtlpJsonExporter::new("http://x/v1/traces", "svc").unwrap();
    let p = exp.render_payload(&[mk(5, "t", SpanStatus::Ok, 1_000)]);
    let obj = p.as_object().unwrap();
    assert!(obj.contains_key("resourceSpans"));
    assert_eq!(exp.name(), "otlp-json");
}

#[test]
fn stdout_exporter_roundtrip() {
    let exp = StdoutExporter::new();
    // Can't easily intercept stdout here; just verify render_line works.
    let line = StdoutExporter::render_line(&mk(5, "t", SpanStatus::Ok, 1_000)).unwrap();
    assert!(line.contains("\"t\":\"span\""));
    // Exporter itself returns Ok for empty batch (no stdout write).
    assert!(exp.export_batch(&[]).is_ok());
}

#[test]
fn jsonrpc_frame_size_cap_enforced() {
    let huge = json!({ "jsonrpc": "2.0", "id": 1, "method": "x", "params": { "p": "a".repeat(2000) } });
    let raw = serde_json::to_vec(&huge).unwrap();
    let err = Frame::parse_with_limit(&raw, 1024);
    assert!(err.is_err());
}

#[test]
fn slo_p95_with_real_jsonl_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = JsonlStore::open(tmp.path()).unwrap();
    let base: u128 = 5_000_000_000;
    for i in 0..90 {
        store
            .record(&mk(50, "t", SpanStatus::Ok, base + i as u128))
            .unwrap();
    }
    for i in 0..10 {
        store
            .record(&mk(500, "t", SpanStatus::Ok, base + 100 + i as u128))
            .unwrap();
    }
    store.flush().unwrap();

    let spans = read_jsonl(tmp.path()).unwrap();
    let toml = r#"
[[slo]]
name = "fast"
metric = "latency_p95_ms"
target = 100
window = "10s"
burn_rate_threshold = 2.0
"#;
    let slos = parse_config_str(toml).unwrap();
    let r = evaluate_all(&slos, &spans, base + 1_000);
    assert!(r[0].burning);
    assert!(r[0].actual >= 500.0);
}

#[test]
fn jsonl_store_is_lf_only() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = JsonlStore::open(tmp.path()).unwrap();
    store.record(&mk(5, "t", SpanStatus::Ok, 1_000)).unwrap();
    let bytes = std::fs::read(tmp.path()).unwrap();
    assert!(!bytes.contains(&b'\r'));
}
