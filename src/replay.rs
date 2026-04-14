//! Replay captured spans to an exporter.
//!
//! Offline mode: given a `.jsonl` file produced by [`crate::store::JsonlStore`],
//! push each span through one or more exporters in insertion order.
//! Used for "my prod proxy recorded spans to S3, I want to forward them
//! to Zipkin now" workflows and for end-to-end tests.

use crate::error::Result;
use crate::exporter::Exporter;
use crate::span::Span;
use crate::store::read_jsonl;
use std::path::Path;
use std::sync::Arc;

/// Load spans from a JSONL file.
pub fn load_spans(path: impl AsRef<Path>) -> Result<Vec<Span>> {
    read_jsonl(path)
}

/// Replay all spans in `spans` through every exporter in one batch per
/// exporter. Returns the number of spans shipped (per exporter, they all
/// see the same count).
pub fn replay_to(spans: &[Span], exporters: &[Arc<dyn Exporter>]) -> Result<usize> {
    if spans.is_empty() {
        return Ok(0);
    }
    for ex in exporters {
        ex.export_batch(spans)?;
    }
    Ok(spans.len())
}

/// Convenience: load from file and replay in one call.
pub fn replay_file(
    path: impl AsRef<Path>,
    exporters: &[Arc<dyn Exporter>],
) -> Result<usize> {
    let spans = load_spans(path)?;
    replay_to(&spans, exporters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::Exporter;
    use crate::span::{Span, SpanStatus};
    use crate::store::{JsonlStore, SpanStore};
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct CaptureExporter {
        batches: Mutex<Vec<Vec<Span>>>,
    }

    impl Exporter for CaptureExporter {
        fn name(&self) -> &'static str {
            "capture"
        }
        fn export_batch(&self, spans: &[Span]) -> Result<()> {
            self.batches.lock().unwrap().push(spans.to_vec());
            Ok(())
        }
    }

    fn sample(id: &str) -> Span {
        Span::builder()
            .trace_id("t")
            .span_id(id)
            .method("tools/call")
            .tool_name(Some("x".into()))
            .duration_ms(5)
            .status(SpanStatus::Ok)
            .build()
            .unwrap()
    }

    #[test]
    fn replay_empty_returns_zero() {
        let cap = Arc::new(CaptureExporter::default()) as Arc<dyn Exporter>;
        let n = replay_to(&[], std::slice::from_ref(&cap)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn replay_pushes_once_per_exporter() {
        let cap1 = Arc::new(CaptureExporter::default());
        let cap2 = Arc::new(CaptureExporter::default());
        let exps: Vec<Arc<dyn Exporter>> = vec![cap1.clone(), cap2.clone()];
        let spans = vec![sample("a"), sample("b"), sample("c")];
        let n = replay_to(&spans, &exps).unwrap();
        assert_eq!(n, 3);
        assert_eq!(cap1.batches.lock().unwrap().len(), 1);
        assert_eq!(cap2.batches.lock().unwrap().len(), 1);
        assert_eq!(cap1.batches.lock().unwrap()[0].len(), 3);
    }

    #[test]
    fn replay_file_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        store.record(&sample("a")).unwrap();
        store.record(&sample("b")).unwrap();
        store.flush().unwrap();

        let cap = Arc::new(CaptureExporter::default());
        let exps: Vec<Arc<dyn Exporter>> = vec![cap.clone()];
        let n = replay_file(tmp.path(), &exps).unwrap();
        assert_eq!(n, 2);
        let batches = cap.batches.lock().unwrap();
        assert_eq!(batches[0][0].span_id, "a");
        assert_eq!(batches[0][1].span_id, "b");
    }

    #[test]
    fn load_spans_missing_file() {
        assert!(load_spans("/tmp/definitely/missing/x.jsonl").is_err());
    }

    #[test]
    fn replay_propagates_exporter_error() {
        struct FailingExporter;
        impl Exporter for FailingExporter {
            fn name(&self) -> &'static str {
                "fail"
            }
            fn export_batch(&self, _: &[Span]) -> Result<()> {
                Err(crate::error::Error::Http("mock".into()))
            }
        }
        let exps: Vec<Arc<dyn Exporter>> = vec![Arc::new(FailingExporter)];
        assert!(replay_to(&[sample("a")], &exps).is_err());
    }
}
