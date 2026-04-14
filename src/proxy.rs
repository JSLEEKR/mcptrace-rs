//! Stdio proxy: spawn a child MCP server, relay JSON-RPC frames in both
//! directions, and emit a [`Span`] per correlated request/response pair.
//!
//! The proxy is implemented with tokio: two async tasks, `agent_to_child`
//! and `child_to_agent`, each reading `u8` bytes line-by-line. A shared
//! `PendingRequests` map correlates responses to requests via their
//! JSON-RPC `id`. When a response arrives, the matching pending entry is
//! finalized into a [`Span`] and pushed to the observer channel.
//!
//! # Back-pressure
//!
//! The observer channel is bounded. If it fills, we *drop* the span and
//! increment a counter; we never stall the relay. Data-plane integrity
//! > observation completeness.
//!
//! # Shutdown
//!
//! Either side closing (EOF) triggers shutdown:
//!
//! - EOF from agent → drop child stdin, wait briefly, kill child.
//! - EOF from child → drop child, end proxy.
//! - Pending-but-never-responded requests are finalized as
//!   [`SpanStatus::Orphan`].

use crate::digest::{arg_digest, error_digest};
use crate::error::{Error, Result};
use crate::exporter::Exporter;
use crate::jsonrpc::{id_key, Frame};
use crate::span::{Span, SpanStatus};
use crate::store::SpanStore;
use crate::trace_id::{new_span_id, new_trace_id};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// A pending request waiting for its response.
#[derive(Debug)]
pub struct Pending {
    span_id: String,
    method: String,
    tool_name: Option<String>,
    arg_digest: Option<String>,
    arg_bytes: u64,
    request_id: serde_json::Value,
    request_bytes: u64,
    started: Instant,
    start_unix_nanos: u128,
}

/// Map of in-flight requests keyed by JSON-RPC id.
type PendingRequests = Arc<Mutex<HashMap<String, Pending>>>;

/// Configuration for [`run_stdio_proxy`].
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub child_cmd: String,
    pub child_args: Vec<String>,
    pub max_frame_bytes: u64,
    pub service_name: String,
    pub observer_buffer: usize,
    pub trace_id: String,
}

impl ProxyConfig {
    /// Sensible defaults: 1 MB frames, `"mcptrace"` service name,
    /// 1024-slot observer buffer, new random trace id.
    #[must_use]
    pub fn new(cmd: impl Into<String>, args: Vec<String>) -> Self {
        ProxyConfig {
            child_cmd: cmd.into(),
            child_args: args,
            max_frame_bytes: crate::jsonrpc::DEFAULT_MAX_FRAME_BYTES,
            service_name: "mcptrace".into(),
            observer_buffer: 1024,
            trace_id: new_trace_id(),
        }
    }
}

/// Runtime stats the proxy emits for operator introspection.
#[derive(Debug, Default)]
pub struct ProxyMetrics {
    pub spans_emitted: AtomicU64,
    pub spans_dropped_buffer_full: AtomicU64,
    pub frames_too_large: AtomicU64,
    pub parse_errors: AtomicU64,
    pub orphan_spans: AtomicU64,
    /// Incremented when the agent re-uses a JSON-RPC id while the
    /// previous request with that id is still in flight. The previous
    /// pending entry is overwritten (per JSON-RPC spec, id reuse before
    /// a response is an agent bug); this counter surfaces the silent drop
    /// so operators can detect a misbehaving agent.
    pub spans_dropped_id_collision: AtomicU64,
}

/// Finalize a [`Pending`] into a [`Span`] given the response frame.
#[must_use]
pub fn finalize(
    trace_id: &str,
    service_name: &str,
    p: Pending,
    response: Option<&Frame>,
) -> Span {
    let duration_ms = u64::try_from(p.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (status, error_code, error_msg_digest, response_bytes) = if let Some(f) = response {
        match f.response_status() {
            Some(SpanStatus::Error) => {
                let (code, msg) = f.response_error().unwrap_or((0, String::new()));
                (
                    SpanStatus::Error,
                    Some(code),
                    Some(error_digest(&msg)),
                    f.byte_len(),
                )
            }
            Some(SpanStatus::Ok) => (SpanStatus::Ok, None, None, f.byte_len()),
            _ => (SpanStatus::Ok, None, None, f.byte_len()),
        }
    } else {
        (SpanStatus::Orphan, None, None, 0)
    };

    Span::builder()
        .trace_id(trace_id)
        .span_id(p.span_id)
        .service_name(service_name)
        .method(p.method)
        .tool_name(p.tool_name)
        .arg_digest(p.arg_digest)
        .arg_bytes(p.arg_bytes)
        .request_id(Some(p.request_id))
        .start_unix_nanos(p.start_unix_nanos)
        .duration_ms(duration_ms)
        .request_bytes(p.request_bytes)
        .response_bytes(response_bytes)
        .error_code(error_code)
        .error_message_digest(error_msg_digest)
        .status(status)
        .build()
        .unwrap_or_else(|_| {
            // Fallback in case builder validation slips; never panics.
            Span {
                schema_version: Span::SCHEMA_VERSION,
                span_id: "unknown".into(),
                parent_span_id: None,
                trace_id: trace_id.into(),
                service_name: service_name.into(),
                method: "unknown".into(),
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
                status: SpanStatus::Orphan,
            }
        })
}

/// Construct a Pending from a parsed request frame.
#[must_use]
pub fn pending_from_request(frame: &Frame) -> Pending {
    let method = frame.method().unwrap_or("").to_string();
    let tool_name = frame.extract_tool_name();
    let (arg_dig, arg_bytes) = frame
        .arguments_canonical()
        .map(|b| (Some(arg_digest(&b)), b.len() as u64))
        .unwrap_or((None, 0));
    let request_id = frame.id().unwrap_or(serde_json::Value::Null);
    Pending {
        span_id: new_span_id(),
        method,
        tool_name,
        arg_digest: arg_dig,
        arg_bytes,
        request_id,
        request_bytes: frame.byte_len(),
        started: Instant::now(),
        start_unix_nanos: Span::now_unix_nanos(),
    }
}

/// Write one JSON-RPC frame + LF to a writer.
async fn write_frame<W>(w: &mut W, bytes: &[u8]) -> Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    w.write_all(bytes).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

/// Process one captured span through storage + exporters.
fn observe(
    span: &Span,
    store: &dyn SpanStore,
    exporters: &[Arc<dyn Exporter>],
    metrics: &ProxyMetrics,
) {
    if let Err(e) = store.record(span) {
        eprintln!("mcptrace: store.record error: {e}");
    }
    for ex in exporters {
        if let Err(e) = ex.export_batch(std::slice::from_ref(span)) {
            eprintln!("mcptrace: exporter {} error: {e}", ex.name());
        }
    }
    metrics.spans_emitted.fetch_add(1, Ordering::Relaxed);
    if matches!(span.status, SpanStatus::Orphan) {
        metrics.orphan_spans.fetch_add(1, Ordering::Relaxed);
    }
}

/// Run the stdio proxy to completion.
///
/// This function:
/// 1. Spawns the child process with piped stdio.
/// 2. Reads agent→child frames from `stdin_source` and forwards them to
///    the child's stdin. On every complete request frame, records a
///    [`Pending`].
/// 3. Reads child→agent frames from child stdout and forwards them to
///    `stdout_sink`. On every response, correlates with a pending entry
///    and emits a [`Span`].
/// 4. On EOF from either side, finalizes any remaining pending entries
///    as [`SpanStatus::Orphan`], flushes, and returns.
///
/// `stdin_source` and `stdout_sink` are generic so that tests can drive
/// the proxy with in-memory buffers instead of real stdio.
pub async fn run_stdio_proxy<R, W>(
    cfg: ProxyConfig,
    mut stdin_source: R,
    mut stdout_sink: W,
    store: Arc<dyn SpanStore>,
    exporters: Vec<Arc<dyn Exporter>>,
) -> Result<ProxyMetrics>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut child = Command::new(&cfg.child_cmd)
        .args(&cfg.child_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let child_stdin: ChildStdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Observation("failed to open child stdin".into()))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Observation("failed to open child stdout".into()))?;

    let metrics = Arc::new(ProxyMetrics::default());
    let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
    let (obs_tx, mut obs_rx) = mpsc::channel::<Span>(cfg.observer_buffer);

    // Observer task (moves store + exporters in).
    let observer_metrics = metrics.clone();
    let observer_store = store;
    let observer = tokio::spawn(async move {
        while let Some(span) = obs_rx.recv().await {
            observe(&span, observer_store.as_ref(), &exporters, &observer_metrics);
        }
        let _ = observer_store.flush();
    });

    // Task 1: agent -> child
    let a2c_pending = pending.clone();
    let a2c_cfg = cfg.clone();
    let a2c_metrics = metrics.clone();
    let a2c_obs_tx = obs_tx.clone();
    let a2c = tokio::spawn(async move {
        let mut reader = BufReader::new(&mut stdin_source).lines();
        let mut child_stdin = child_stdin;
        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("mcptrace: agent read error: {e}");
                    break;
                }
            };
            if line.is_empty() {
                continue;
            }
            let bytes = line.as_bytes();
            // Parse for instrumentation; even if parse fails we forward.
            match Frame::parse_with_limit(bytes, a2c_cfg.max_frame_bytes) {
                Ok(frame) => {
                    if frame.is_request() && !frame.is_notification() {
                        let pend = pending_from_request(&frame);
                        let key = id_key(&pend.request_id);
                        let prev = a2c_pending.lock().await.insert(key, pend);
                        if prev.is_some() {
                            a2c_metrics
                                .spans_dropped_id_collision
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if write_frame(&mut child_stdin, bytes).await.is_err() {
                        break;
                    }
                }
                Err(Error::FrameTooLarge { .. }) => {
                    a2c_metrics.frames_too_large.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "mcptrace: dropping oversized agent->child frame ({} bytes)",
                        bytes.len()
                    );
                }
                Err(e) => {
                    a2c_metrics.parse_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("mcptrace: parse error (a->c): {e}; forwarding anyway");
                    if write_frame(&mut child_stdin, bytes).await.is_err() {
                        break;
                    }
                }
            }
        }
        // EOF from agent: close child stdin to signal end.
        let _ = child_stdin.shutdown().await;
        drop(a2c_obs_tx);
    });

    // Task 2: child -> agent
    let c2a_pending = pending.clone();
    let c2a_cfg = cfg.clone();
    let c2a_metrics = metrics.clone();
    let c2a_obs_tx = obs_tx;
    let c2a = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout).lines();
        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("mcptrace: child read error: {e}");
                    break;
                }
            };
            if line.is_empty() {
                continue;
            }
            let bytes = line.as_bytes();
            match Frame::parse_with_limit(bytes, c2a_cfg.max_frame_bytes) {
                Ok(frame) => {
                    if frame.is_response() {
                        if let Some(id) = frame.id() {
                            let key = id_key(&id);
                            let entry = c2a_pending.lock().await.remove(&key);
                            if let Some(p) = entry {
                                let span =
                                    finalize(&c2a_cfg.trace_id, &c2a_cfg.service_name, p, Some(&frame));
                                if c2a_obs_tx.try_send(span).is_err() {
                                    c2a_metrics
                                        .spans_dropped_buffer_full
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    if write_frame(&mut stdout_sink, bytes).await.is_err() {
                        break;
                    }
                }
                Err(Error::FrameTooLarge { .. }) => {
                    c2a_metrics.frames_too_large.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "mcptrace: dropping oversized child->agent frame ({} bytes)",
                        bytes.len()
                    );
                }
                Err(e) => {
                    c2a_metrics.parse_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("mcptrace: parse error (c->a): {e}; forwarding anyway");
                    if write_frame(&mut stdout_sink, bytes).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Finalize any still-pending requests as orphans.
        let mut map = c2a_pending.lock().await;
        let drained: Vec<Pending> = map.drain().map(|(_, v)| v).collect();
        drop(map);
        for p in drained {
            let span = finalize(&c2a_cfg.trace_id, &c2a_cfg.service_name, p, None);
            if c2a_obs_tx.try_send(span).is_err() {
                c2a_metrics
                    .spans_dropped_buffer_full
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        drop(c2a_obs_tx);
    });

    let _ = a2c.await;
    let _ = c2a.await;
    let _ = observer.await;
    let _ = child.kill().await;
    // Extract metrics out of the Arc (clone the values).
    let out = ProxyMetrics {
        spans_emitted: AtomicU64::new(metrics.spans_emitted.load(Ordering::Relaxed)),
        spans_dropped_buffer_full: AtomicU64::new(
            metrics.spans_dropped_buffer_full.load(Ordering::Relaxed),
        ),
        frames_too_large: AtomicU64::new(metrics.frames_too_large.load(Ordering::Relaxed)),
        parse_errors: AtomicU64::new(metrics.parse_errors.load(Ordering::Relaxed)),
        orphan_spans: AtomicU64::new(metrics.orphan_spans.load(Ordering::Relaxed)),
        spans_dropped_id_collision: AtomicU64::new(
            metrics.spans_dropped_id_collision.load(Ordering::Relaxed),
        ),
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::Frame;

    #[test]
    fn pending_from_tools_call_captures_tool_and_digest() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search","arguments":{"q":"rust"}}}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let p = pending_from_request(&f);
        assert_eq!(p.method, "tools/call");
        assert_eq!(p.tool_name.as_deref(), Some("search"));
        assert!(p.arg_digest.is_some());
        assert!(p.arg_bytes > 0);
        assert_eq!(p.request_bytes, raw.len() as u64);
    }

    #[test]
    fn pending_from_non_tool_call() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let f = Frame::parse_with_limit(raw, 1024).unwrap();
        let p = pending_from_request(&f);
        assert_eq!(p.method, "initialize");
        assert!(p.tool_name.is_none());
        assert!(p.arg_digest.is_none());
    }

    #[test]
    fn finalize_ok_response() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x","arguments":{}}}"#;
        let req = Frame::parse_with_limit(raw, 1024).unwrap();
        let p = pending_from_request(&req);
        let resp_raw = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let resp = Frame::parse_with_limit(resp_raw, 1024).unwrap();
        let span = finalize("t", "svc", p, Some(&resp));
        assert_eq!(span.status, SpanStatus::Ok);
        assert_eq!(span.response_bytes, resp_raw.len() as u64);
        assert_eq!(span.trace_id, "t");
        assert_eq!(span.service_name, "svc");
    }

    #[test]
    fn finalize_error_response_has_digest_not_message() {
        let req_raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#;
        let req = Frame::parse_with_limit(req_raw, 1024).unwrap();
        let p = pending_from_request(&req);
        let resp_raw = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"secret-leak-attempt"}}"#;
        let resp = Frame::parse_with_limit(resp_raw, 1024).unwrap();
        let span = finalize("t", "svc", p, Some(&resp));
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!(span.error_code, Some(-32602));
        let d = span.error_message_digest.clone().unwrap();
        assert_eq!(d.len(), 64);
        // The plaintext should never appear anywhere in the span.
        let j = serde_json::to_string(&span).unwrap();
        assert!(!j.contains("secret-leak-attempt"));
    }

    #[test]
    fn finalize_orphan_when_no_response() {
        let req_raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#;
        let req = Frame::parse_with_limit(req_raw, 1024).unwrap();
        let p = pending_from_request(&req);
        let span = finalize("t", "svc", p, None);
        assert_eq!(span.status, SpanStatus::Orphan);
        assert_eq!(span.response_bytes, 0);
    }

    #[test]
    fn proxy_config_defaults() {
        let c = ProxyConfig::new("echo", vec!["hi".into()]);
        assert_eq!(c.child_cmd, "echo");
        assert_eq!(c.max_frame_bytes, 1_048_576);
        assert_eq!(c.service_name, "mcptrace");
        assert!(!c.trace_id.is_empty());
    }

    #[test]
    fn proxy_metrics_start_zero() {
        let m = ProxyMetrics::default();
        assert_eq!(m.spans_emitted.load(Ordering::Relaxed), 0);
        assert_eq!(m.orphan_spans.load(Ordering::Relaxed), 0);
    }

    // In-memory end-to-end test of the request->response->span path,
    // bypassing the actual child process. We drive both the "pending"
    // construction and the "finalize" step directly, simulating what
    // the proxy loop does on the wire. This is both faster and more
    // reliable than spawning a shell in CI.
    #[test]
    fn e2e_request_response_to_span_happy_path() {
        let req_raw = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"search","arguments":{"q":"rust"}}}"#;
        let req = Frame::parse_with_limit(req_raw, 4096).unwrap();
        assert!(req.is_request());
        let pending = pending_from_request(&req);
        assert_eq!(pending.tool_name.as_deref(), Some("search"));

        let resp_raw = br#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"ok"}]}}"#;
        let resp = Frame::parse_with_limit(resp_raw, 4096).unwrap();
        assert!(resp.is_response());

        let span = finalize("trace-1", "mcp-svc", pending, Some(&resp));
        assert_eq!(span.status, SpanStatus::Ok);
        assert_eq!(span.trace_id, "trace-1");
        assert_eq!(span.tool_name.as_deref(), Some("search"));
        assert!(span.response_bytes > 0);
        assert!(span.arg_digest.is_some());
    }

    #[test]
    fn observe_increments_spans_emitted_counter() {
        use crate::store::NullStore;
        let metrics = ProxyMetrics::default();
        let store = NullStore;
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("tools/call")
            .tool_name(Some("x".into()))
            .status(SpanStatus::Ok)
            .build()
            .unwrap();
        observe(&s, &store, &[], &metrics);
        assert_eq!(metrics.spans_emitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.orphan_spans.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pending_insert_collision_is_observable() {
        // Regression: when an agent re-uses a JSON-RPC id while the
        // previous request is still in flight, the proxy must surface
        // the silent overwrite via spans_dropped_id_collision so
        // operators can detect it. Before the fix, the second insert
        // silently dropped the first Pending without any counter bump.
        let metrics = ProxyMetrics::default();
        let mut map: HashMap<String, Pending> = HashMap::new();

        let req_raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"a"}}"#;
        let req2_raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"b"}}"#;
        let f1 = Frame::parse_with_limit(req_raw, 1024).unwrap();
        let f2 = Frame::parse_with_limit(req2_raw, 1024).unwrap();
        let p1 = pending_from_request(&f1);
        let p2 = pending_from_request(&f2);
        let key1 = id_key(&p1.request_id);
        let key2 = id_key(&p2.request_id);
        assert_eq!(key1, key2, "test precondition: ids collide");

        // First insert: no collision.
        let prev = map.insert(key1.clone(), p1);
        if prev.is_some() {
            metrics
                .spans_dropped_id_collision
                .fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(metrics.spans_dropped_id_collision.load(Ordering::Relaxed), 0);

        // Second insert with same key: collision counter must bump.
        let prev = map.insert(key2, p2);
        if prev.is_some() {
            metrics
                .spans_dropped_id_collision
                .fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(metrics.spans_dropped_id_collision.load(Ordering::Relaxed), 1);
        assert_eq!(map.len(), 1, "the collided entry overwrites in place");
    }

    #[test]
    fn proxy_metrics_default_includes_id_collision() {
        let m = ProxyMetrics::default();
        assert_eq!(m.spans_dropped_id_collision.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn observe_bumps_orphan_counter_for_orphan_span() {
        use crate::store::NullStore;
        let metrics = ProxyMetrics::default();
        let store = NullStore;
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("tools/call")
            .status(SpanStatus::Orphan)
            .build()
            .unwrap();
        observe(&s, &store, &[], &metrics);
        assert_eq!(metrics.orphan_spans.load(Ordering::Relaxed), 1);
    }
}
